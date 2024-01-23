/*!
Defining the [Table] struct that represents an iceberg table.
*/

use std::{collections::HashMap, io::Cursor, iter::repeat, sync::Arc, time::SystemTime};

use object_store::{path::Path, ObjectStore};

use futures::{stream, StreamExt, TryFutureExt, TryStreamExt};
use iceberg_rust_spec::spec::{
    manifest::{Content, ManifestEntry, ManifestReader},
    manifest_list::ManifestListEntry,
    schema::Schema,
    snapshot::{Operation, Snapshot, SnapshotReference, SnapshotRetention, Summary},
    table_metadata::{SnapshotLog, TableMetadata, MAIN_BRANCH},
};
use iceberg_rust_spec::util::{self, strip_prefix};

use crate::{
    catalog::{bucket::parse_bucket, identifier::Identifier, Catalog},
    error::Error,
    table::transaction::TableTransaction,
};

pub mod table_builder;
pub mod transaction;

#[derive(Debug)]
/// Iceberg table
pub struct Table {
    identifier: Identifier,
    catalog: Arc<dyn Catalog>,
    metadata: TableMetadata,
    metadata_location: String,
}

/// Public interface of the table.
impl Table {
    /// Create a new metastore Table
    pub async fn new(
        identifier: Identifier,
        catalog: Arc<dyn Catalog>,
        metadata: TableMetadata,
        metadata_location: &str,
    ) -> Result<Self, Error> {
        Ok(Table {
            identifier,
            catalog,
            metadata,
            metadata_location: metadata_location.to_string(),
        })
    }
    #[inline]
    /// Get the table identifier in the catalog. Returns None of it is a filesystem table.
    pub fn identifier(&self) -> &Identifier {
        &self.identifier
    }
    #[inline]
    /// Get the catalog associated to the table. Returns None if the table is a filesystem table
    pub fn catalog(&self) -> Arc<dyn Catalog> {
        self.catalog.clone()
    }
    #[inline]
    /// Get the object_store associated to the table
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        self.catalog
            .object_store(parse_bucket(&self.metadata.location).unwrap())
    }
    #[inline]
    /// Get the schema of the table for a given branch. Defaults to main.
    pub fn current_schema(&self, branch: Option<&str>) -> Result<&Schema, Error> {
        self.metadata.current_schema(branch).map_err(Error::from)
    }
    #[inline]
    /// Get the metadata of the table
    pub fn metadata(&self) -> &TableMetadata {
        &self.metadata
    }
    #[inline]
    /// Get the location of the current metadata file
    pub fn metadata_location(&self) -> &str {
        &self.metadata_location
    }
    /// Get list of current manifest files within an optional snapshot range. The start snapshot is excluded from the range.
    pub async fn manifests(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<Vec<ManifestListEntry>, Error> {
        let metadata = self.metadata();
        let end_snapshot = match end.and_then(|id| metadata.snapshots.get(&id)) {
            Some(snapshot) => snapshot,
            None => {
                if let Some(current) = metadata.current_snapshot(None)? {
                    current
                } else {
                    return Ok(vec![]);
                }
            }
        };
        let start_sequence_number =
            start
                .and_then(|id| metadata.snapshots.get(&id))
                .and_then(|snapshot| {
                    let sequence_number = snapshot.sequence_number;
                    if sequence_number == 0 {
                        None
                    } else {
                        Some(sequence_number)
                    }
                });
        let iter = end_snapshot
            .manifests(metadata, self.object_store().clone())
            .await?;
        match start_sequence_number {
            Some(start) => iter
                .filter(|manifest| {
                    if let Ok(manifest) = manifest {
                        manifest.sequence_number > start
                    } else {
                        true
                    }
                })
                .collect::<Result<_, iceberg_rust_spec::error::Error>>()
                .map_err(Error::from),
            None => iter
                .collect::<Result<_, iceberg_rust_spec::error::Error>>()
                .map_err(Error::from),
        }
    }
    /// Get list of datafiles corresponding to the given manifest files
    pub async fn datafiles(
        &self,
        manifests: &[ManifestListEntry],
        filter: Option<Vec<bool>>,
    ) -> Result<Vec<ManifestEntry>, Error> {
        // filter manifest files according to filter vector
        let iter = match filter {
            Some(predicate) => {
                manifests
                    .iter()
                    .zip(Box::new(predicate.into_iter())
                        as Box<dyn Iterator<Item = bool> + Send + Sync>)
                    .filter_map(
                        filter_manifest
                            as fn((&ManifestListEntry, bool)) -> Option<&ManifestListEntry>,
                    )
            }
            None => manifests
                .iter()
                .zip(Box::new(repeat(true)) as Box<dyn Iterator<Item = bool> + Send + Sync>)
                .filter_map(
                    filter_manifest as fn((&ManifestListEntry, bool)) -> Option<&ManifestListEntry>,
                ),
        };
        // Collect a vector of data files by creating a stream over the manifst files, fetch their content and return a flatten stream over their entries.
        stream::iter(iter)
            .map(|file| async move {
                let object_store = Arc::clone(&self.object_store());
                let path: Path = util::strip_prefix(&file.manifest_path).into();
                let bytes = Cursor::new(Vec::from(
                    object_store
                        .get(&path)
                        .and_then(|file| file.bytes())
                        .await?,
                ));
                let reader = ManifestReader::new(bytes)?;
                Ok(stream::iter(reader))
            })
            .flat_map(|reader| reader.try_flatten_stream())
            .try_collect()
            .await
            .map_err(Error::from)
    }
    /// Check if datafiles contain deletes
    pub async fn datafiles_contains_delete(
        &self,
        start: Option<i64>,
        end: Option<i64>,
    ) -> Result<bool, Error> {
        let manifests = self.manifests(start, end).await?;
        let datafiles = self.datafiles(&manifests, None).await?;
        Ok(datafiles
            .iter()
            .any(|entry| !matches!(entry.data_file.content, Content::Data)))
    }
    /// Create a new transaction for this table
    pub fn new_transaction(&mut self, branch: Option<&str>) -> TableTransaction {
        TableTransaction::new(self, branch)
    }

    /// delete all datafiles, manifests and metadata files, does not remove table from catalog
    pub async fn drop(self) -> Result<(), Error> {
        let object_store = self.object_store();
        let manifests = self.manifests(None, None).await?;
        let datafiles = self.datafiles(&manifests, None).await?;
        let snapshots = &self.metadata().snapshots;

        stream::iter(datafiles.into_iter())
            .map(Ok::<_, Error>)
            .try_for_each_concurrent(None, |datafile| {
                let object_store = object_store.clone();
                async move {
                    object_store
                        .delete(&datafile.data_file.file_path.into())
                        .await?;
                    Ok(())
                }
            })
            .await?;

        stream::iter(manifests.into_iter())
            .map(Ok::<_, Error>)
            .try_for_each_concurrent(None, |manifest| {
                let object_store = object_store.clone();
                async move {
                    object_store.delete(&manifest.manifest_path.into()).await?;
                    Ok(())
                }
            })
            .await?;

        stream::iter(snapshots.values())
            .map(Ok::<_, Error>)
            .try_for_each_concurrent(None, |snapshot| {
                let object_store = object_store.clone();
                async move {
                    object_store
                        .delete(&snapshot.manifest_list.as_str().into())
                        .await?;
                    Ok(())
                }
            })
            .await?;

        object_store
            .delete(&self.metadata_location().into())
            .await?;

        Ok(())
    }
}

/// Private interface of the table.
impl Table {
    #[inline]
    /// Increment the sequence number of the table. Is typically used when commiting a new table transaction.
    pub(crate) fn increment_sequence_number(&mut self) {
        self.metadata.last_sequence_number += 1;
    }

    /// Create a new table snapshot based on the manifest_list file of the previous snapshot.
    pub(crate) async fn new_snapshot(
        &mut self,
        branch: Option<String>,
    ) -> Result<Option<Vec<u8>>, Error> {
        let mut bytes: [u8; 8] = [0u8; 8];
        getrandom::getrandom(&mut bytes).unwrap();
        let snapshot_id = u64::from_le_bytes(bytes) as i64;
        let object_store = self.object_store();
        let parent_snapshot_id = branch
            .as_deref()
            .map(|x| self.metadata.refs.get(x).map(|x| x.snapshot_id))
            .unwrap_or(self.metadata.current_snapshot_id);
        let metadata = &mut self.metadata;
        let old_manifest_list_location = metadata
            .current_snapshot(branch.as_deref())?
            .map(|x| &x.manifest_list)
            .cloned();
        let new_manifest_list_location = metadata.location.to_string()
            + "/metadata/snap-"
            + &snapshot_id.to_string()
            + &uuid::Uuid::new_v4().to_string()
            + ".avro";
        let timestamp_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let snapshot = Snapshot {
            snapshot_id,
            parent_snapshot_id,
            sequence_number: metadata.last_sequence_number,
            timestamp_ms,
            manifest_list: new_manifest_list_location,
            summary: Summary {
                operation: Operation::Append,
                other: HashMap::new(),
            },
            schema_id: Some(metadata.current_schema_id),
        };

        let branch_name = branch.unwrap_or("main".to_string());

        metadata.snapshots.insert(snapshot_id, snapshot);
        if branch_name == MAIN_BRANCH {
            metadata.current_snapshot_id = Some(snapshot_id);
        }
        metadata.snapshot_log.push(SnapshotLog {
            snapshot_id,
            timestamp_ms,
        });
        metadata
            .refs
            .entry(branch_name)
            .and_modify(|x| x.snapshot_id = snapshot_id)
            .or_insert(SnapshotReference {
                snapshot_id,
                retention: SnapshotRetention::default(),
            });
        match old_manifest_list_location {
            Some(old_manifest_list_location) => Ok(Some(
                object_store
                    .get(&strip_prefix(&old_manifest_list_location).as_str().into())
                    .await?
                    .bytes()
                    .await?
                    .into(),
            )),
            None => Ok(None),
        }
    }
}

#[inline]
// Filter manifest files according to predicate. Returns Some(&ManifestFile) of the predicate is true and None if it is false.
fn filter_manifest(
    (manifest, predicate): (&ManifestListEntry, bool),
) -> Option<&ManifestListEntry> {
    if predicate {
        Some(manifest)
    } else {
        None
    }
}

/*!
 * Defines the different [Operation]s on a [Table].
*/

use std::{collections::HashMap, sync::Arc};

use iceberg_rust_spec::manifest_list::{
    manifest_list_schema_v1, manifest_list_schema_v2, ManifestListReader,
};
use iceberg_rust_spec::spec::table_metadata::TableMetadata;
use iceberg_rust_spec::spec::{
    manifest::{partition_value_schema, DataFile, ManifestEntry, Status},
    schema::Schema,
    snapshot::{
        generate_snapshot_id, SnapshotBuilder, SnapshotReference, SnapshotRetention, Summary,
    },
};
use iceberg_rust_spec::table_metadata::FormatVersion;
use iceberg_rust_spec::util::strip_prefix;
use object_store::ObjectStore;
use smallvec::SmallVec;

use crate::table::manifest::{ManifestReader, ManifestWriter};
use crate::{
    catalog::commit::{TableRequirement, TableUpdate},
    error::Error,
    util::{partition_struct_to_vec, summary_to_rectangle, Rectangle},
};

use super::append::{
    select_manifest_partitioned, select_manifest_unpartitioned, split_datafiles, SelectedManifest,
};

/// The target number of datafiles per manifest is dynamic, but we don't want to go below this number.
static MIN_DATAFILES_PER_MANIFEST: usize = 4;

/// To achieve fast lookups of the datafiles, the manifest tree should be somewhat balanced, meaning that manifest files should contain a similar number of datafiles.
/// This means that manifest files might need to be split up when they get too large. Since the number of datafiles being added by a append operation might be really large,
/// it might even be required to split the manifest file multiple times. *n_splits* stores how many times a manifest file needs to be split to give at most *limit* datafiles per manifest.
fn compute_n_splits(
    existing_file_count: usize,
    new_file_count: usize,
    selected_manifest_file_count: usize,
) -> u32 {
    // We want:
    //   nb manifests per manifest list ~= nb data files per manifest
    // Since:
    //   total number of data files = nb manifests per manifest list * nb data files per manifest
    // We shall have:
    //   limit = sqrt(total number of data files)
    let limit = MIN_DATAFILES_PER_MANIFEST
        + ((existing_file_count + new_file_count) as f64).sqrt() as usize;
    let new_manifest_file_count = selected_manifest_file_count + new_file_count;
    match new_manifest_file_count / limit {
        0 => 0,
        x => x.ilog2() + 1,
    }
}

#[derive(Debug)]
///Table operations
pub enum Operation {
    /// Update schema
    AddSchema(Schema),
    /// Update spec
    SetDefaultSpec(i32),
    /// Update table properties
    UpdateProperties(Vec<(String, String)>),
    /// Set Ref
    SetSnapshotRef((String, SnapshotReference)),
    /// Replace the sort order
    // ReplaceSortOrder,
    // /// Update the table location
    // UpdateLocation,
    /// Append new files to the table
    Append {
        branch: Option<String>,
        files: Vec<DataFile>,
        additional_summary: Option<HashMap<String, String>>,
    },
    // /// Quickly append new files to the table
    // NewFastAppend {
    //     paths: Vec<String>,
    //     partition_values: Vec<Struct>,
    // },
    // /// Replace files in the table and commit
    Rewrite {
        branch: Option<String>,
        files: Vec<DataFile>,
        additional_summary: Option<HashMap<String, String>>,
    },
    // /// Replace manifests files and commit
    // RewriteManifests,
    // /// Replace files in the table by a filter expression
    // NewOverwrite,
    // /// Remove or replace rows in existing data files
    // NewRowDelta,
    // /// Delete files in the table and commit
    // NewDelete,
    // /// Expire snapshots in the table
    // ExpireSnapshots,
    // /// Manage snapshots in the table
    // ManageSnapshots,
    // /// Read and write table data and metadata files
    // IO,
}

impl Operation {
    pub async fn execute(
        self,
        table_metadata: &TableMetadata,
        object_store: Arc<dyn ObjectStore>,
    ) -> Result<(Option<TableRequirement>, Vec<TableUpdate>), Error> {
        match self {
            Operation::Append {
                branch,
                files: new_files,
                additional_summary,
            } => {
                let partition_spec = table_metadata.default_partition_spec()?;
                let schema = table_metadata.current_schema(branch.as_deref())?;
                let old_snapshot = table_metadata.current_snapshot(branch.as_deref())?;

                let partition_column_names = table_metadata
                    .default_partition_spec()?
                    .fields()
                    .iter()
                    .map(|x| x.name().as_str())
                    .collect::<SmallVec<[_; 4]>>();

                let bounding_partition_values = new_files
                    .iter()
                    .try_fold(None, |acc, x| {
                        let node = partition_struct_to_vec(x.partition(), &partition_column_names)?;
                        let Some(mut acc) = acc else {
                            return Ok::<_, Error>(Some(Rectangle::new(node.clone(), node)));
                        };
                        acc.expand_with_node(node);
                        Ok(Some(acc))
                    })?
                    .ok_or(Error::NotFound(
                        "Bounding".to_owned(),
                        "rectangle".to_owned(),
                    ))?;

                let manifest_list_schema = match table_metadata.format_version {
                    FormatVersion::V1 => manifest_list_schema_v1(),
                    FormatVersion::V2 => manifest_list_schema_v2(),
                };

                let mut manifest_list_writer =
                    apache_avro::Writer::new(manifest_list_schema, Vec::new());

                let old_manifest_list_location = old_snapshot.map(|x| x.manifest_list()).cloned();

                // Find a manifest to add the new datafiles
                let mut existing_file_count = 0;
                let selected_manifest_opt = if let Some(old_manifest_list_location) =
                    &old_manifest_list_location
                {
                    let old_manifest_list_bytes = object_store
                        .get(&strip_prefix(old_manifest_list_location).as_str().into())
                        .await?
                        .bytes()
                        .await?;

                    let manifest_list_reader =
                        ManifestListReader::new(old_manifest_list_bytes.as_ref(), table_metadata)?;

                    let SelectedManifest {
                        manifest,
                        file_count_all_entries,
                    } = if partition_column_names.is_empty() {
                        select_manifest_unpartitioned(
                            manifest_list_reader,
                            &mut manifest_list_writer,
                        )?
                    } else {
                        select_manifest_partitioned(
                            manifest_list_reader,
                            &mut manifest_list_writer,
                            &bounding_partition_values,
                        )?
                    };
                    existing_file_count = file_count_all_entries;
                    Some(manifest)
                } else {
                    // If manifest list doesn't exist, there is no manifest
                    None
                };

                let selected_manifest_file_count = selected_manifest_opt
                    .as_ref()
                    // TODO: should this also account for existing_files_count?
                    .and_then(|selected_manifest| selected_manifest.v1.added_files_count)
                    .unwrap_or(0) as usize;
                let n_splits = compute_n_splits(
                    existing_file_count,
                    new_files.len(),
                    selected_manifest_file_count,
                );

                let bounds = selected_manifest_opt
                    .as_ref()
                    .and_then(|x| x.v1.partitions.as_deref())
                    .map(summary_to_rectangle)
                    .transpose()?
                    .map(|mut x| {
                        x.expand(&bounding_partition_values);
                        x
                    })
                    .unwrap_or(bounding_partition_values);

                let snapshot_id = generate_snapshot_id();
                let sequence_number = table_metadata.last_sequence_number + 1;

                let new_datafile_iter = new_files.into_iter().map(|data_file| {
                    ManifestEntry::builder()
                        .with_format_version(table_metadata.format_version)
                        .with_status(Status::Added)
                        .with_snapshot_id(snapshot_id)
                        .with_sequence_number(sequence_number)
                        .with_data_file(data_file)
                        .build()
                        .map_err(crate::spec::error::Error::from)
                        .map_err(Error::from)
                });

                let manifest_schema = ManifestEntry::schema(
                    &partition_value_schema(partition_spec.fields(), schema)?,
                    &table_metadata.format_version,
                )?;

                let snapshot_uuid = &uuid::Uuid::new_v4().to_string();
                let new_manifest_list_location = table_metadata.location.to_string()
                    + "/metadata/snap-"
                    + &snapshot_id.to_string()
                    + snapshot_uuid
                    + ".avro";

                // Write manifest files
                // Split manifest file if limit is exceeded
                if n_splits == 0 {
                    let mut manifest_writer = if let Some(manifest) = selected_manifest_opt {
                        let manifest_bytes: Vec<u8> = object_store
                            .get(&strip_prefix(&manifest.v1.manifest_path).as_str().into())
                            .await?
                            .bytes()
                            .await?
                            .into();

                        ManifestWriter::from_existing(
                            &manifest_bytes,
                            manifest,
                            &manifest_schema,
                            table_metadata,
                            branch.as_deref(),
                        )?
                    } else {
                        let manifest_location = table_metadata.location.to_string()
                            + "/metadata/"
                            + snapshot_uuid
                            + "-m"
                            + &0.to_string()
                            + ".avro";

                        ManifestWriter::new(
                            &manifest_location,
                            snapshot_id,
                            &manifest_schema,
                            table_metadata,
                            branch.as_deref(),
                        )?
                    };

                    for manifest_entry in new_datafile_iter {
                        manifest_writer.append(manifest_entry?)?;
                    }

                    let manifest = manifest_writer.finish(object_store.clone()).await?;

                    manifest_list_writer.append_ser(manifest)?;
                } else {
                    // Split datafiles
                    let splits = if let Some(manifest) = selected_manifest_opt {
                        let manifest_bytes: Vec<u8> = object_store
                            .get(&strip_prefix(&manifest.v1.manifest_path).as_str().into())
                            .await?
                            .bytes()
                            .await?
                            .into();

                        let manifest_reader =
                            ManifestReader::new(&*manifest_bytes)?.map(|x| x.map_err(Error::from));

                        split_datafiles(
                            new_datafile_iter.chain(manifest_reader),
                            bounds,
                            &partition_column_names,
                            n_splits,
                        )?
                    } else {
                        split_datafiles(
                            new_datafile_iter,
                            bounds,
                            &partition_column_names,
                            n_splits,
                        )?
                    };

                    for (i, entries) in splits.into_iter().enumerate() {
                        let manifest_location = table_metadata.location.to_string()
                            + "/metadata/"
                            + snapshot_uuid
                            + "-m"
                            + &i.to_string()
                            + ".avro";

                        let mut manifest_writer = ManifestWriter::new(
                            &manifest_location,
                            snapshot_id,
                            &manifest_schema,
                            table_metadata,
                            branch.as_deref(),
                        )?;

                        for manifest_entry in entries {
                            manifest_writer.append(manifest_entry)?;
                        }

                        let manifest = manifest_writer.finish(object_store.clone()).await?;

                        manifest_list_writer.append_ser(manifest)?;
                    }
                };

                let manifest_list_bytes = manifest_list_writer.into_inner()?;

                object_store
                    .put(
                        &strip_prefix(&new_manifest_list_location).into(),
                        manifest_list_bytes.into(),
                    )
                    .await?;

                let mut snapshot_builder = SnapshotBuilder::default();
                snapshot_builder
                    .with_snapshot_id(snapshot_id)
                    .with_manifest_list(new_manifest_list_location)
                    .with_sequence_number(
                        old_snapshot
                            .map(|x| *x.sequence_number() + 1)
                            .unwrap_or_default(),
                    )
                    .with_summary(Summary {
                        operation: iceberg_rust_spec::spec::snapshot::Operation::Append,
                        other: additional_summary.unwrap_or_default(),
                    })
                    .with_schema_id(*schema.schema_id());
                let snapshot = snapshot_builder
                    .build()
                    .map_err(iceberg_rust_spec::error::Error::from)?;

                Ok((
                    old_snapshot.map(|x| TableRequirement::AssertRefSnapshotId {
                        r#ref: branch.clone().unwrap_or("main".to_owned()),
                        snapshot_id: *x.snapshot_id(),
                    }),
                    vec![
                        TableUpdate::AddSnapshot { snapshot },
                        TableUpdate::SetSnapshotRef {
                            ref_name: branch.unwrap_or("main".to_owned()),
                            snapshot_reference: SnapshotReference {
                                snapshot_id,
                                retention: SnapshotRetention::default(),
                            },
                        },
                    ],
                ))
            }
            Operation::Rewrite {
                branch,
                files,
                additional_summary,
            } => {
                let partition_spec = table_metadata.default_partition_spec()?;
                let old_snapshot = table_metadata.current_snapshot(branch.as_deref())?;
                let schema = table_metadata.current_schema(branch.as_deref())?.clone();

                let partition_column_names = table_metadata
                    .default_partition_spec()?
                    .fields()
                    .iter()
                    .map(|x| x.name().as_str())
                    .collect::<SmallVec<[_; 4]>>();

                let bounding_partition_values = files
                    .iter()
                    .try_fold(None, |acc, x| {
                        let node = partition_struct_to_vec(x.partition(), &partition_column_names)?;
                        let Some(mut acc) = acc else {
                            return Ok::<_, Error>(Some(Rectangle::new(node.clone(), node)));
                        };
                        acc.expand_with_node(node);
                        Ok(Some(acc))
                    })?
                    .ok_or(Error::NotFound(
                        "Bounding".to_owned(),
                        "rectangle".to_owned(),
                    ))?;

                let manifest_list_schema = match table_metadata.format_version {
                    FormatVersion::V1 => manifest_list_schema_v1(),
                    FormatVersion::V2 => manifest_list_schema_v2(),
                };

                let mut manifest_list_writer =
                    apache_avro::Writer::new(manifest_list_schema, Vec::new());

                let n_splits = compute_n_splits(0, files.len(), 0);

                let snapshot_id = generate_snapshot_id();
                let sequence_number = table_metadata.last_sequence_number + 1;

                let new_datafile_iter = files.into_iter().map(|data_file| {
                    ManifestEntry::builder()
                        .with_format_version(table_metadata.format_version)
                        .with_status(Status::Added)
                        .with_snapshot_id(snapshot_id)
                        .with_sequence_number(sequence_number)
                        .with_data_file(data_file)
                        .build()
                        .map_err(crate::spec::error::Error::from)
                        .map_err(Error::from)
                });

                let manifest_schema = ManifestEntry::schema(
                    &partition_value_schema(partition_spec.fields(), &schema)?,
                    &table_metadata.format_version,
                )?;

                let snapshot_uuid = &uuid::Uuid::new_v4().to_string();
                let new_manifest_list_location = table_metadata.location.to_string()
                    + "/metadata/snap-"
                    + &snapshot_id.to_string()
                    + snapshot_uuid
                    + ".avro";

                // Write manifest files
                // Split manifest file if limit is exceeded
                if n_splits == 0 {
                    // If manifest doesn't need to be split

                    let manifest_location = table_metadata.location.to_string()
                        + "/metadata/"
                        + snapshot_uuid
                        + "-m"
                        + &0.to_string()
                        + ".avro";
                    let mut manifest_writer = ManifestWriter::new(
                        &manifest_location,
                        snapshot_id,
                        &manifest_schema,
                        table_metadata,
                        branch.as_deref(),
                    )?;

                    for manifest_entry in new_datafile_iter {
                        manifest_writer.append(manifest_entry?)?;
                    }

                    let manifest = manifest_writer.finish(object_store.clone()).await?;

                    manifest_list_writer.append_ser(manifest)?;
                } else {
                    // Split datafiles
                    let splits = split_datafiles(
                        new_datafile_iter,
                        bounding_partition_values,
                        &partition_column_names,
                        n_splits,
                    )?;

                    for (i, entries) in splits.into_iter().enumerate() {
                        let manifest_location = table_metadata.location.to_string()
                            + "/metadata/"
                            + snapshot_uuid
                            + "-m"
                            + &i.to_string()
                            + ".avro";

                        let mut manifest_writer = ManifestWriter::new(
                            &manifest_location,
                            snapshot_id,
                            &manifest_schema,
                            table_metadata,
                            branch.as_deref(),
                        )?;

                        for manifest_entry in entries {
                            manifest_writer.append(manifest_entry)?;
                        }

                        let manifest = manifest_writer.finish(object_store.clone()).await?;

                        manifest_list_writer.append_ser(manifest)?;
                    }
                };

                let manifest_list_bytes = manifest_list_writer.into_inner()?;

                object_store
                    .put(
                        &strip_prefix(&new_manifest_list_location).into(),
                        manifest_list_bytes.into(),
                    )
                    .await?;

                let mut snapshot_builder = SnapshotBuilder::default();
                snapshot_builder
                    .with_snapshot_id(snapshot_id)
                    .with_sequence_number(0)
                    .with_schema_id(*schema.schema_id())
                    .with_manifest_list(new_manifest_list_location)
                    .with_summary(Summary {
                        operation: iceberg_rust_spec::spec::snapshot::Operation::Append,
                        other: additional_summary.unwrap_or_default(),
                    });
                let snapshot = snapshot_builder
                    .build()
                    .map_err(iceberg_rust_spec::error::Error::from)?;

                let old_snapshot_ids: Vec<i64> =
                    table_metadata.snapshots.keys().map(Clone::clone).collect();

                Ok((
                    old_snapshot.map(|x| TableRequirement::AssertRefSnapshotId {
                        r#ref: branch.clone().unwrap_or("main".to_owned()),
                        snapshot_id: *x.snapshot_id(),
                    }),
                    vec![
                        TableUpdate::RemoveSnapshots {
                            snapshot_ids: old_snapshot_ids,
                        },
                        TableUpdate::AddSnapshot { snapshot },
                        TableUpdate::SetSnapshotRef {
                            ref_name: branch.unwrap_or("main".to_owned()),
                            snapshot_reference: SnapshotReference {
                                snapshot_id,
                                retention: SnapshotRetention::default(),
                            },
                        },
                    ],
                ))
            }
            Operation::UpdateProperties(entries) => Ok((
                None,
                vec![TableUpdate::SetProperties {
                    updates: HashMap::from_iter(entries),
                }],
            )),
            Operation::SetSnapshotRef((key, value)) => Ok((
                table_metadata
                    .refs
                    .get(&key)
                    .map(|x| TableRequirement::AssertRefSnapshotId {
                        r#ref: key.clone(),
                        snapshot_id: x.snapshot_id,
                    }),
                vec![TableUpdate::SetSnapshotRef {
                    ref_name: key,
                    snapshot_reference: value,
                }],
            )),
            Operation::AddSchema(schema) => {
                let last_column_id = schema.fields().iter().map(|x| x.id).max();
                Ok((
                    None,
                    vec![TableUpdate::AddSchema {
                        schema,
                        last_column_id,
                    }],
                ))
            }
            Operation::SetDefaultSpec(spec_id) => {
                Ok((None, vec![TableUpdate::SetDefaultSpec { spec_id }]))
            }
        }
    }
}

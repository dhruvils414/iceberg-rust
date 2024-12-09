/*!
 * Helpers to deal with manifest files
*/

use std::{
    io::Read,
    iter::{repeat, Map, Repeat, Zip},
    sync::Arc,
};

use apache_avro::{
    types::Value as AvroValue, Reader as AvroReader, Schema as AvroSchema, Writer as AvroWriter,
};
use iceberg_rust_spec::{
    manifest::{ManifestEntry, ManifestEntryV1, ManifestEntryV2},
    manifest_list::{self, FieldSummary, ManifestListEntry},
    partition::{PartitionField, PartitionSpec},
    schema::{Schema, SchemaV1, SchemaV2},
    table_metadata::{FormatVersion, TableMetadata},
    util::strip_prefix,
    values::{Struct, Value},
};
use object_store::ObjectStore;

use crate::{error::Error, spec};

type ReaderZip<'a, R> = Zip<AvroReader<'a, R>, Repeat<Arc<(Schema, PartitionSpec, FormatVersion)>>>;
type ReaderMap<'a, R> = Map<
    ReaderZip<'a, R>,
    fn(
        (
            Result<AvroValue, apache_avro::Error>,
            Arc<(Schema, PartitionSpec, FormatVersion)>,
        ),
    ) -> Result<ManifestEntry, Error>,
>;

/// Iterator of ManifestFileEntries
pub struct ManifestReader<'a, R: Read> {
    reader: ReaderMap<'a, R>,
}

impl<'a, R: Read> Iterator for ManifestReader<'a, R> {
    type Item = Result<ManifestEntry, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next()
    }
}

impl<'a, R: Read> ManifestReader<'a, R> {
    /// Create a new ManifestFile reader
    pub fn new(reader: R) -> Result<Self, Error> {
        let reader = AvroReader::new(reader)?;
        let metadata = reader.user_metadata();

        let format_version: FormatVersion = match metadata
            .get("format-version")
            .map(|bytes| String::from_utf8(bytes.clone()))
            .transpose()?
            .unwrap_or("1".to_string())
            .as_str()
        {
            "1" => Ok(FormatVersion::V1),
            "2" => Ok(FormatVersion::V2),
            _ => Err(Error::InvalidFormat("format version".to_string())),
        }?;

        let schema: Schema = match format_version {
            FormatVersion::V1 => TryFrom::<SchemaV1>::try_from(serde_json::from_slice(
                metadata
                    .get("schema")
                    .ok_or(Error::InvalidFormat("manifest metadata".to_string()))?,
            )?)?,
            FormatVersion::V2 => TryFrom::<SchemaV2>::try_from(serde_json::from_slice(
                metadata
                    .get("schema")
                    .ok_or(Error::InvalidFormat("manifest metadata".to_string()))?,
            )?)?,
        };

        let partition_fields: Vec<PartitionField> = serde_json::from_slice(
            metadata
                .get("partition-spec")
                .ok_or(Error::InvalidFormat("manifest metadata".to_string()))?,
        )?;
        let spec_id: i32 = metadata
            .get("partition-spec-id")
            .map(|x| String::from_utf8(x.clone()))
            .transpose()?
            .unwrap_or("0".to_string())
            .parse()?;
        let partition_spec = PartitionSpec::builder()
            .with_spec_id(spec_id)
            .with_fields(partition_fields)
            .build()
            .map_err(spec::error::Error::from)?;
        Ok(Self {
            reader: reader
                .zip(repeat(Arc::new((schema, partition_spec, format_version))))
                .map(avro_value_to_manifest_entry),
        })
    }
}

/// A helper to write entries into a manifest
pub struct ManifestWriter<'schema, 'metadata> {
    table_metadata: &'metadata TableMetadata,
    manifest: ManifestListEntry,
    writer: AvroWriter<'schema, Vec<u8>>,
}
use crate::spec::manifest_list::_serde::ManifestListEntryV1;

impl<'schema, 'metadata> ManifestWriter<'schema, 'metadata> {
    /// Create empty manifest writer
    pub fn new(
        manifest_location: &str,
        snapshot_id: i64,
        schema: &'schema AvroSchema,
        table_metadata: &'metadata TableMetadata,
        branch: Option<&str>,
    ) -> Result<Self, Error> {
        let mut writer = AvroWriter::new(schema, Vec::new());

        writer.add_user_metadata(
            "format-version".to_string(),
            match table_metadata.format_version {
                FormatVersion::V1 => "1".as_bytes(),
                FormatVersion::V2 => "2".as_bytes(),
            },
        )?;

        writer.add_user_metadata(
            "schema".to_string(),
            match table_metadata.format_version {
                FormatVersion::V1 => serde_json::to_string(&Into::<SchemaV1>::into(
                    table_metadata.current_schema(branch)?.clone(),
                ))?,
                FormatVersion::V2 => serde_json::to_string(&Into::<SchemaV2>::into(
                    table_metadata.current_schema(branch)?.clone(),
                ))?,
            },
        )?;

        writer.add_user_metadata(
            "schema-id".to_string(),
            serde_json::to_string(&table_metadata.current_schema(branch)?.schema_id())?,
        )?;

        writer.add_user_metadata(
            "partition-spec".to_string(),
            serde_json::to_string(&table_metadata.default_partition_spec()?.fields())?,
        )?;

        writer.add_user_metadata(
            "partition-spec-id".to_string(),
            serde_json::to_string(&table_metadata.default_partition_spec()?.spec_id())?,
        )?;

        writer.add_user_metadata("content".to_string(), "data")?;

        let manifest = ManifestListEntry {
            format_version: table_metadata.format_version.clone(),
            content: manifest_list::Content::Data,
            sequence_number: table_metadata.last_sequence_number,
            min_sequence_number: 0,
            v1: ManifestListEntryV1 {
                added_snapshot_id: snapshot_id,
                manifest_path: manifest_location.to_owned(),
                manifest_length: 0,
                partition_spec_id: table_metadata.default_spec_id,
                added_files_count: Some(0),
                existing_files_count: Some(0),
                deleted_files_count: Some(0),
                added_rows_count: Some(0),
                existing_rows_count: Some(0),
                deleted_rows_count: Some(0),
                partitions: None,
                key_metadata: None,
            },
        };

        Ok(ManifestWriter {
            manifest,
            writer,
            table_metadata,
        })
    }

    /// Create an manifest writer from an existing manifest
    pub fn from_existing(
        bytes: &[u8],
        manifest: ManifestListEntry,
        schema: &'schema AvroSchema,
        table_metadata: &'metadata TableMetadata,
        branch: Option<&str>,
    ) -> Result<Self, Error> {
        let manifest_reader = apache_avro::Reader::new(bytes)?;

        let mut writer = AvroWriter::new(schema, Vec::new());

        writer.add_user_metadata(
            "format-version".to_string(),
            match table_metadata.format_version {
                FormatVersion::V1 => "1".as_bytes(),
                FormatVersion::V2 => "2".as_bytes(),
            },
        )?;

        writer.add_user_metadata(
            "schema".to_string(),
            match table_metadata.format_version {
                FormatVersion::V1 => serde_json::to_string(&Into::<SchemaV1>::into(
                    table_metadata.current_schema(branch)?.clone(),
                ))?,
                FormatVersion::V2 => serde_json::to_string(&Into::<SchemaV2>::into(
                    table_metadata.current_schema(branch)?.clone(),
                ))?,
            },
        )?;

        writer.add_user_metadata(
            "schema-id".to_string(),
            serde_json::to_string(&table_metadata.current_schema(branch)?.schema_id())?,
        )?;

        writer.add_user_metadata(
            "partition-spec".to_string(),
            serde_json::to_string(&table_metadata.default_partition_spec()?.fields())?,
        )?;

        writer.add_user_metadata(
            "partition-spec-id".to_string(),
            serde_json::to_string(&table_metadata.default_partition_spec()?.spec_id())?,
        )?;

        writer.add_user_metadata("content".to_string(), "data")?;

        writer.extend(manifest_reader.filter_map(Result::ok))?;

        Ok(ManifestWriter {
            manifest,
            writer,
            table_metadata,
        })
    }

    /// Add an manifest entry to the manifest
    pub fn append(&mut self, manifest_entry: ManifestEntry) -> Result<(), Error> {
        let mut added_rows_count = 0;

        if self.manifest.v1.partitions.is_none() {
            self.manifest.v1.partitions = Some(
                self.table_metadata
                    .default_partition_spec()?
                    .fields()
                    .iter()
                    .map(|_| FieldSummary {
                        contains_null: false,
                        contains_nan: None,
                        lower_bound: None,
                        upper_bound: None,
                    })
                    .collect::<Vec<FieldSummary>>(),
            );
        }

        added_rows_count += manifest_entry.data_file().record_count();
        update_partitions(
            self.manifest.v1.partitions.as_mut().unwrap(),
            manifest_entry.data_file().partition(),
            self.table_metadata.default_partition_spec()?.fields(),
        )?;

        self.writer.append_ser(manifest_entry)?;

        self.manifest.v1.added_files_count = match self.manifest.v1.added_files_count {
            Some(count) => Some(count + 1),
            None => Some(1),
        };
        self.manifest.v1.added_rows_count = match self.manifest.v1.added_rows_count {
            Some(count) => Some(count + added_rows_count),
            None => Some(added_rows_count),
        };

        Ok(())
    }

    /// Write the manifest to object storage and return the manifest-list-entry
    pub async fn finish(
        mut self,
        object_store: Arc<dyn ObjectStore>,
    ) -> Result<ManifestListEntry, Error> {
        let manifest_bytes = self.writer.into_inner()?;

        let manifest_length: i64 = manifest_bytes.len() as i64;

        self.manifest.v1.manifest_length += manifest_length;

        object_store
            .put(
                &strip_prefix(&self.manifest.v1.manifest_path)
                    .as_str()
                    .into(),
                manifest_bytes.into(),
            )
            .await?;
        Ok(self.manifest)
    }
}

#[allow(clippy::type_complexity)]
/// Convert avro value to ManifestEntry based on the format version of the table.
pub fn avro_value_to_manifest_entry(
    value: (
        Result<AvroValue, apache_avro::Error>,
        Arc<(Schema, PartitionSpec, FormatVersion)>,
    ),
) -> Result<ManifestEntry, Error> {
    let entry = value.0?;
    let schema = &value.1 .0;
    let partition_spec = &value.1 .1;
    let format_version = &value.1 .2;
    match format_version {
        FormatVersion::V2 => ManifestEntry::try_from_v2(
            apache_avro::from_value::<ManifestEntryV2>(&entry)?,
            schema,
            partition_spec,
        )
        .map_err(Error::from),
        FormatVersion::V1 => ManifestEntry::try_from_v1(
            apache_avro::from_value::<ManifestEntryV1>(&entry)?,
            schema,
            partition_spec,
        )
        .map_err(Error::from),
    }
}

fn update_partitions(
    partitions: &mut [FieldSummary],
    partition_values: &Struct,
    partition_columns: &[PartitionField],
) -> Result<(), Error> {
    for (field, summary) in partition_columns.iter().zip(partitions.iter_mut()) {
        let value = partition_values.get(field.name()).and_then(|x| x.as_ref());
        if let Some(value) = value {
            if summary.lower_bound.is_none() {
                summary.lower_bound = Some(value.clone());
            } else if let Some(lower_bound) = &mut summary.lower_bound {
                match (value, lower_bound) {
                    (Value::Int(val), Value::Int(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::LongInt(val), Value::LongInt(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::Float(val), Value::Float(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::Double(val), Value::Double(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::Date(val), Value::Date(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::Time(val), Value::Time(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::Timestamp(val), Value::Timestamp(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    (Value::TimestampTZ(val), Value::TimestampTZ(current)) => {
                        if *current > *val {
                            *current = *val
                        }
                    }
                    _ => {}
                }
            }
            if summary.upper_bound.is_none() {
                summary.upper_bound = Some(value.clone());
            } else if let Some(upper_bound) = &mut summary.upper_bound {
                match (value, upper_bound) {
                    (Value::Int(val), Value::Int(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::LongInt(val), Value::LongInt(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::Float(val), Value::Float(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::Double(val), Value::Double(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::Date(val), Value::Date(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::Time(val), Value::Time(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::Timestamp(val), Value::Timestamp(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    (Value::TimestampTZ(val), Value::TimestampTZ(current)) => {
                        if *current < *val {
                            *current = *val
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// TODO
#[cfg(test)]
mod tests {}

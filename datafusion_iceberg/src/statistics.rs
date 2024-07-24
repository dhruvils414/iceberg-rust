use std::ops::Deref;

use datafusion::{
    common::stats::Precision,
    physical_plan::{ColumnStatistics, Statistics},
    scalar::ScalarValue,
};
use iceberg_rust::{catalog::tabular::Tabular, table::Table};
use iceberg_rust_spec::spec::{manifest::{ManifestEntry, Status}, schema::Schema, values::Value};

use crate::error::Error;

use super::table::DataFusionTable;

impl DataFusionTable {
    pub(crate) async fn statistics(&self) -> Result<Statistics, Error> {
        let table_read = self.tabular.read().await;
        match table_read.deref() {
            Tabular::Table(table) => table_statistics(table, &self.snapshot_range).await,
            Tabular::View(_) => Err(Error::NotSupported("Statistics for views".to_string())),
            Tabular::MaterializedView(mv) => {
                let table = mv.storage_table().await.map_err(Error::from)?;
                table_statistics(&table, &self.snapshot_range).await
            }
        }
    }
}

pub(crate) async fn table_statistics(
    table: &Table,
    snapshot_range: &(Option<i64>, Option<i64>),
) -> Result<Statistics, Error> {
    eprintln!("Inside table statistics function");
    let schema = snapshot_range
        .1
        .and_then(|snapshot_id| table.metadata().schema(snapshot_id).ok().cloned())
        .unwrap_or_else(|| table.current_schema(None).unwrap().clone());
    let manifests = table.manifests(snapshot_range.0, snapshot_range.1).await?;
    let datafiles = table.datafiles(&manifests, None).await?;
    let file_groups: Vec<ManifestEntry> = datafiles.into_iter().filter(|manifest| {
        if *manifest.status() == Status::Deleted { false } else { true }
    }).collect();

    Ok(file_groups.iter().fold(
        Statistics {
            num_rows: Precision::Exact(0),
            total_byte_size: Precision::Exact(0),
            column_statistics: vec![
                ColumnStatistics {
                    null_count: Precision::Absent,
                    max_value: Precision::Absent,
                    min_value: Precision::Absent,
                    distinct_count: Precision::Absent
                };
                schema.fields().len()
            ],
        },
        |acc, manifest| {
                let column_stats = column_statistics(&schema, manifest);
                Statistics {
                num_rows: acc.num_rows.add(&Precision::Exact(
                    *manifest.data_file().record_count() as usize
                )),
                total_byte_size: acc.total_byte_size.add(&Precision::Exact(
                    *manifest.data_file().file_size_in_bytes() as usize,
                )),
                column_statistics: acc
                    .column_statistics
                    .into_iter()
                    .zip(column_stats)
                    .map(|(acc, x)| ColumnStatistics {
                        null_count: acc.null_count.add(&x.null_count),
                        max_value: acc.max_value.max(&x.max_value),
                        min_value: acc.min_value.min(&x.min_value),
                        distinct_count: acc.distinct_count.add(&x.distinct_count),
                    })
                    .collect(),
                }
        },
    ))
}

fn column_statistics<'a>(
    schema: &'a Schema,
    manifest: &'a ManifestEntry,
) -> impl Iterator<Item = ColumnStatistics> + 'a {
    schema.fields().iter().map(|x| x.id).map(|id| {
        let data_file = &manifest.data_file();
        ColumnStatistics {
            null_count: data_file
                .null_value_counts()
                .as_ref()
                .and_then(|x| x.get(&id))
                .map(|x| Precision::Exact(*x as usize))
                .unwrap_or(Precision::Absent),
            max_value: data_file
                .upper_bounds()
                .as_ref()
                .and_then(|x| x.get(&id))
                .and_then(|x| {
                    Some(Precision::Exact(
                        convert_value_to_scalar_value(x.clone()).ok()?,
                    ))
                })
                .unwrap_or(Precision::Absent),
            min_value: data_file
                .lower_bounds()
                .as_ref()
                .and_then(|x| x.get(&id))
                .and_then(|x| {
                    Some(Precision::Exact(
                        convert_value_to_scalar_value(x.clone()).ok()?,
                    ))
                })
                .unwrap_or(Precision::Absent),
            distinct_count: data_file
                .distinct_counts()
                .as_ref()
                .and_then(|x| x.get(&id))
                .map(|x| Precision::Exact(*x as usize))
                .unwrap_or(Precision::Absent),
        }
    })
}

pub(crate) fn manifest_statistics(schema: &Schema, manifest: &ManifestEntry) -> Statistics {
    Statistics {
        num_rows: Precision::Exact(*manifest.data_file().record_count() as usize),
        total_byte_size: Precision::Exact(*manifest.data_file().file_size_in_bytes() as usize),
        column_statistics: column_statistics(schema, manifest).collect(),
    }
}

fn convert_value_to_scalar_value(value: Value) -> Result<ScalarValue, Error> {
    match value {
        Value::Boolean(b) => Ok(ScalarValue::Boolean(Some(b))),
        Value::Int(i) => Ok(ScalarValue::Int32(Some(i))),
        Value::LongInt(l) => Ok(ScalarValue::Int64(Some(l))),
        Value::Float(f) => Ok(ScalarValue::Float32(Some(f.0))),
        Value::Double(d) => Ok(ScalarValue::Float64(Some(d.0))),
        Value::Date(d) => Ok(ScalarValue::Date32(Some(d))),
        Value::Time(t) => Ok(ScalarValue::Time64Microsecond(Some(t))),
        Value::Timestamp(ts) => Ok(ScalarValue::TimestampMicrosecond(Some(ts), None)),
        Value::TimestampTZ(ts) => Ok(ScalarValue::TimestampMicrosecond(Some(ts), None)),
        Value::String(s) => Ok(ScalarValue::Utf8(Some(s))),
        Value::UUID(u) => Ok(ScalarValue::FixedSizeBinary(
            16,
            Some(u.into_bytes().into()),
        )),
        Value::Fixed(size, data) => Ok(ScalarValue::FixedSizeBinary(size as i32, Some(data))),
        Value::Binary(data) => Ok(ScalarValue::Binary(Some(data))),
        Value::Decimal(decimal) => Ok(ScalarValue::Decimal128(
            Some(decimal.try_into().unwrap()),
            0,
            0,
        )),
        x => Err(Error::Conversion(
            "Iceberg value".to_string(),
            format!("{:?}", x),
        )),
    }
}

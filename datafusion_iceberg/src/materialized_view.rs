use std::sync::Arc;

use datafusion::{
    arrow::error::ArrowError,
    datasource::{empty::EmptyTable, TableProvider},
    prelude::SessionContext,
    sql::TableReference,
};
use futures::{stream, StreamExt, TryStreamExt};
use iceberg_rust::{
    arrow::write::write_parquet_partitioned,
    catalog::{identifier::Identifier, tabular::Tabular, CatalogList},
    materialized_view::{MaterializedView, StorageTableState},
    sql::find_relations,
};
use iceberg_rust_spec::spec::{
    materialized_view_metadata::SourceTable, view_metadata::ViewRepresentation,
};
use itertools::Itertools;

use crate::{
    error::Error,
    sql::{transform_name, transform_relations},
    DataFusionTable,
};

pub async fn refresh_materialized_view(
    matview: &mut MaterializedView,
    catalog_list: Arc<dyn CatalogList>,
    branch: Option<&str>,
) -> Result<(), Error> {
    let ctx = SessionContext::new();

    let sql = match &matview.metadata().current_version(branch)?.representations[0] {
        ViewRepresentation::Sql { sql, dialect: _ } => sql,
    };

    let storage_table = matview.storage_table().await?;

    let branch = branch.map(ToString::to_string);

    let source_tables = match storage_table.source_tables(branch.clone()).await? {
        Some(x) => x.clone(),
        None => find_relations(sql)?
            .into_iter()
            .map(|x| {
                Ok(SourceTable {
                    identifier: x,
                    snapshot_id: -1,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?,
    };

    // Load source tables
    let source_tables = stream::iter(source_tables.iter())
        .then(|source_table| {
            let catalog_list = catalog_list.clone();
            let branch = branch.clone();
            async move {
                let identifier = TableReference::parse_str(&source_table.identifier);
                let catalog_name = identifier
                    .catalog()
                    .ok_or(Error::NotFound(
                        "Catalog in ".to_owned(),
                        source_table.identifier.clone(),
                    ))?
                    .to_string();
                let namespace_name = identifier.schema().ok_or(Error::NotFound(
                    "Namspace in ".to_owned(),
                    source_table.identifier.clone(),
                ))?;
                let catalog = catalog_list
                    .catalog(&catalog_name)
                    .await
                    .ok_or(Error::NotFound(
                        "Catalog".to_owned(),
                        catalog_name.to_owned(),
                    ))?;

                let tabular = match catalog
                    .load_tabular(&Identifier::try_new(&[
                        namespace_name.to_string(),
                        identifier.table().to_string(),
                    ])?)
                    .await?
                {
                    Tabular::View(_) => {
                        return Err(Error::InvalidFormat("storage table".to_string()))
                    }
                    x => x,
                };
                let current_snapshot_id = match &tabular {
                    Tabular::Table(table) => Ok(*table
                        .metadata()
                        .current_snapshot(branch.as_deref())?
                        // Fallback to main branch
                        .or(table.metadata().current_snapshot(None)?)
                        .ok_or(Error::NotFound(
                            "Snapshot in source table".to_owned(),
                            (&identifier.table()).to_string(),
                        ))?
                        .snapshot_id()),
                    Tabular::MaterializedView(mv) => {
                        let storage_table = mv.storage_table().await?;
                        Ok(*storage_table
                            .metadata()
                            .current_snapshot(branch.as_deref())?
                            // Fallback to main branch
                            .or(storage_table.metadata().current_snapshot(None)?)
                            .ok_or(Error::NotFound(
                                "Snapshot in source table".to_owned(),
                                (&identifier.table()).to_string(),
                            ))?
                            .snapshot_id())
                    }
                    _ => Err(Error::InvalidFormat("storage table".to_string())),
                }?;

                let table_state = if current_snapshot_id == source_table.snapshot_id {
                    StorageTableState::Fresh
                } else if source_table.snapshot_id == -1 {
                    StorageTableState::Invalid
                } else {
                    StorageTableState::Outdated(source_table.snapshot_id)
                };

                Ok((catalog_name, tabular, table_state, current_snapshot_id))
            }
        })
        .try_collect::<Vec<_>>()
        .await?;

    if source_tables
        .iter()
        .all(|x| matches!(x.2, StorageTableState::Fresh))
    {
        return Ok(());
    }

    // Register source tables in datafusion context and return lineage information
    let source_tables = source_tables
        .into_iter()
        .flat_map(|(catalog_name, source_table, _, last_snapshot_id)| {
            let identifier = source_table.identifier().to_string().to_owned();

            let table = Arc::new(DataFusionTable::new(
                source_table,
                None,
                None,
                branch.as_deref(),
            )) as Arc<dyn TableProvider>;
            let schema = table.schema().clone();

            vec![
                (
                    catalog_name.clone(),
                    identifier.clone(),
                    last_snapshot_id,
                    table,
                ),
                (
                    catalog_name.clone(),
                    identifier + "__delta__",
                    last_snapshot_id,
                    Arc::new(EmptyTable::new(schema)) as Arc<dyn TableProvider>,
                ),
            ]
        })
        .map(|(catalog_name, identifier, snapshot_id, table)| {
            ctx.register_table(&transform_name(&identifier), table)?;
            Ok::<_, Error>((catalog_name.to_string() + "." + &identifier, snapshot_id))
        })
        .filter_ok(|(identifier, _)| !identifier.ends_with("__delta__"))
        .map(|x| {
            let (identifier, snapshot_id) = x?;
            Ok(SourceTable {
                identifier,
                snapshot_id,
            })
        })
        .collect::<Result<_, Error>>()?;

    let sql_statements = transform_relations(sql)?;

    let logical_plan = ctx.state().create_logical_plan(&sql_statements[0]).await?;

    // Calculate arrow record batches from logical plan
    let batches = ctx
        .execute_logical_plan(logical_plan)
        .await?
        .execute_stream()
        .await?
        .map_err(ArrowError::from);

    // Write arrow record batches to datafiles
    let files = write_parquet_partitioned(
        &storage_table.metadata(),
        batches,
        matview.object_store(),
        branch.as_deref(),
    )
    .await?;

    matview
        .new_transaction(branch.as_deref())
        .full_refresh(files, source_tables)
        .commit()
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {

    use datafusion::{arrow::array::Int64Array, prelude::SessionContext};
    use iceberg_rust::{
        catalog::CatalogList,
        materialized_view::materialized_view_builder::MaterializedViewBuilder,
        table::table_builder::TableBuilder,
    };
    use iceberg_rust_spec::spec::{
        partition::{PartitionField, PartitionSpecBuilder, Transform},
        schema::Schema,
        types::{PrimitiveType, StructField, StructType, Type},
    };
    use iceberg_sql_catalog::SqlCatalogList;
    use object_store::{memory::InMemory, ObjectStore};
    use std::sync::Arc;

    use crate::{catalog::catalog::IcebergCatalog, materialized_view::refresh_materialized_view};

    #[tokio::test]
    pub async fn test_datafusion_refresh_materialized_view() {
        let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

        let catalog_list = Arc::new(
            SqlCatalogList::new("sqlite://",  "inMemory", None)
                .await
                .unwrap(),
        );

        let catalog = catalog_list.catalog("iceberg").await.unwrap();

        let schema = Schema::builder()
            .with_schema_id(1)
            .with_fields(
                StructType::builder()
                    .with_struct_field(StructField {
                        id: 1,
                        name: "id".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 2,
                        name: "customer_id".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 3,
                        name: "product_id".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 4,
                        name: "date".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Date),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 5,
                        name: "amount".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Int),
                        doc: None,
                    })
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let partition_spec = PartitionSpecBuilder::default()
            .with_spec_id(1)
            .with_partition_field(PartitionField::new(4, 1000, "day", Transform::Day))
            .build()
            .expect("Failed to create partition spec");

        let mut builder = TableBuilder::new("test.orders", catalog.clone())
            .expect("Failed to create table builder");
        builder
            .location("/test/orders")
            .with_schema((1, schema.clone()))
            .current_schema_id(1)
            .with_partition_spec((1, partition_spec))
            .default_spec_id(1);

        builder.build().await.expect("Failed to create table.");

        let matview_schema = Schema::builder()
            .with_schema_id(1)
            .with_fields(
                StructType::builder()
                    .with_struct_field(StructField {
                        id: 1,
                        name: "product_id".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 2,
                        name: "amount".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Int),
                        doc: None,
                    })
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let mut builder = MaterializedViewBuilder::new(
            "select product_id, amount from iceberg.test.orders where product_id < 3;",
            "test.orders_view",
            matview_schema,
            catalog.clone(),
        )
        .expect("Failed to create filesystem view builder.");
        builder.location("test/orders_view");
        let mut matview = builder
            .build()
            .await
            .expect("Failed to create filesystem view");
        let total_matview_schema = Schema::builder()
            .with_schema_id(1)
            .with_fields(
                StructType::builder()
                    .with_struct_field(StructField {
                        id: 1,
                        name: "product_id".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 2,
                        name: "amount".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::Long),
                        doc: None,
                    })
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let mut total_builder = MaterializedViewBuilder::new(
            "select product_id, sum(amount) from iceberg.test.orders_view group by product_id;",
            "test.total_orders",
            total_matview_schema,
            catalog.clone(),
        )
        .expect("Failed to create filesystem view builder.");
        total_builder.location("test/total_orders");
        let mut total_matview = total_builder
            .build()
            .await
            .expect("Failed to create filesystem view");

        // Datafusion

        let datafusion_catalog = Arc::new(
            IcebergCatalog::new(catalog, None)
                .await
                .expect("Failed to create datafusion catalog"),
        );

        let ctx = SessionContext::new();

        ctx.register_catalog("iceberg", datafusion_catalog);

        ctx.sql(
            "INSERT INTO iceberg.test.orders (id, customer_id, product_id, date, amount) VALUES 
                (1, 1, 1, '2020-01-01', 1),
                (2, 2, 1, '2020-01-01', 1),
                (3, 3, 1, '2020-01-01', 3),
                (4, 1, 2, '2020-02-02', 1),
                (5, 1, 1, '2020-02-02', 2),
                (6, 3, 3, '2020-02-02', 3);",
        )
        .await
        .expect("Failed to create query plan for insert")
        .collect()
        .await
        .expect("Failed to insert values into table");

        refresh_materialized_view(&mut matview, catalog_list.clone(), None)
            .await
            .expect("Failed to refresh materialized view");

        let batches = ctx
            .sql(
                "select product_id, sum(amount) from iceberg.test.orders_view group by product_id;",
            )
            .await
            .expect("Failed to create plan for select")
            .collect()
            .await
            .expect("Failed to execute select query");

        for batch in batches {
            if batch.num_rows() != 0 {
                let (order_ids, amounts) = (
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                );
                for (order_id, amount) in order_ids.iter().zip(amounts) {
                    if order_id.unwrap() == 1 {
                        assert_eq!(amount.unwrap(), 7)
                    } else if order_id.unwrap() == 2 {
                        assert_eq!(amount.unwrap(), 1)
                    } else {
                        panic!("Unexpected order id")
                    }
                }
            }
        }

        ctx.sql(
            "INSERT INTO iceberg.test.orders (id, customer_id, product_id, date, amount) VALUES 
                (7, 1, 3, '2020-01-03', 1),
                (8, 2, 1, '2020-01-03', 2),
                (9, 2, 2, '2020-01-03', 1);",
        )
        .await
        .expect("Failed to create query plan for insert")
        .collect()
        .await
        .expect("Failed to insert values into table");

        refresh_materialized_view(&mut matview, catalog_list.clone(), None)
            .await
            .expect("Failed to refresh materialized view");

        let batches = ctx
            .sql(
                "select product_id, sum(amount) from iceberg.test.orders_view group by product_id;",
            )
            .await
            .expect("Failed to create plan for select")
            .collect()
            .await
            .expect("Failed to execute select query");

        for batch in batches {
            if batch.num_rows() != 0 {
                let (order_ids, amounts) = (
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                );
                for (order_id, amount) in order_ids.iter().zip(amounts) {
                    if order_id.unwrap() == 1 {
                        assert_eq!(amount.unwrap(), 9)
                    } else if order_id.unwrap() == 2 {
                        assert_eq!(amount.unwrap(), 2)
                    } else {
                        panic!("Unexpected order id")
                    }
                }
            }
        }

        refresh_materialized_view(&mut total_matview, catalog_list.clone(), None)
            .await
            .expect("Failed to refresh materialized view");

        let batches = ctx
            .sql("select product_id, amount from iceberg.test.total_orders;")
            .await
            .expect("Failed to create plan for select")
            .collect()
            .await
            .expect("Failed to execute select query");

        for batch in batches {
            if batch.num_rows() != 0 {
                let (order_ids, amounts) = (
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap(),
                );
                for (order_id, amount) in order_ids.iter().zip(amounts) {
                    if order_id.unwrap() == 1 {
                        assert_eq!(amount.unwrap(), 9)
                    } else if order_id.unwrap() == 2 {
                        assert_eq!(amount.unwrap(), 2)
                    } else {
                        panic!("Unexpected order id")
                    }
                }
            }
        }
    }
}

use datafusion::{prelude::SessionContext};
use datafusion_iceberg::DataFusionTable;
use iceberg_rust::{
    catalog::Catalog,
    spec::{
        partition::{PartitionField, PartitionSpecBuilder, Transform},
        schema::Schema,
        types::{PrimitiveType, StructField, StructType, Type},
    },
    table::table_builder::TableBuilder,
};
use iceberg_sql_catalog::SqlCatalog;
use object_store::memory::InMemory;
use object_store::ObjectStore;

use std::sync::Arc;

#[tokio::main]
pub(crate) async fn main() {
    let object_store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());

    let catalog: Arc<dyn Catalog> = Arc::new(
        SqlCatalog::new("sqlite://", "test", object_store.clone())
            .await
            .unwrap(),
    );

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

    let mut builder =
        TableBuilder::new("test.orders", catalog).expect("Failed to create table builder");

    builder
        .location("/test/orders")
        .with_schema((1, schema))
        .current_schema_id(1)
        .with_partition_spec((1, partition_spec))
        .default_spec_id(1);

    let table = Arc::new(DataFusionTable::from(
        builder.build().await.expect("Failed to create table."),
    ));

    let ctx = SessionContext::new();

    ctx.register_table("orders", table).unwrap();

    let insert_plan = ctx.sql(
        "INSERT INTO orders (id, customer_id, product_id, date, amount) VALUES 
        (1, 1, 1, '2020-01-01', 1),
        (2, 2, 1, '2020-01-01', 1),
        (3, 3, 1, '2020-01-01', 3),
        (4, 1, 2, '2020-02-02', 1),
        (5, 1, 1, '2020-02-02', 2),
        (6, 3, 3, '2020-02-02', 3);",
    )
    .await
    .expect("Failed to create query plan for insert");

    // println!("{:#?}", insert_plan);
    
    let _physical_imp = insert_plan.collect()
    .await
    .expect("Failed to insert values into table");

    // println!("Physical Plan {:#?}", physical_imp);

    // let batches = ctx
    //     .sql("select product_id, sum(amount) from orders group by product_id;")
    //     .await
    //     .expect("Failed to create plan for select")
    //     .collect()
    //     .await
    //     .expect("Failed to execute select query");

    // for batch in batches {
    //     if batch.num_rows() != 0 {
    //         let (product_ids, amounts) = (
    //             batch
    //                 .column(0)
    //                 .as_any()
    //                 .downcast_ref::<Int64Array>()
    //                 .unwrap(),
    //             batch
    //                 .column(1)
    //                 .as_any()
    //                 .downcast_ref::<Int64Array>()
    //                 .unwrap(),
    //         );
    //         for (product_id, amount) in product_ids.iter().zip(amounts) {
    //             if product_id.unwrap() == 1 {
    //                 assert_eq!(amount.unwrap(), 7)
    //             } else if product_id.unwrap() == 2 {
    //                 assert_eq!(amount.unwrap(), 1)
    //             } else if product_id.unwrap() == 3 {
    //                 assert_eq!(amount.unwrap(), 3)
    //             } else {
    //                 panic!("Unexpected product id")
    //             }
    //         }
    //     }
    // }

    let updated_dataframe = ctx.sql(
        "SELECT * from orders WHERE customer_id=1;"
    ).await.expect("Failed to create SELECT Query Plan");


    updated_dataframe.show().await.expect("msg");

    // UPDATE Query Sample
    // let dataframe = ctx.sql("
    // EXPLAIN UPDATE orders \
    // SET \
    //     amount=10 \
    // WHERE \
    //     customer_id=1
    // ").await.expect("Failed to create query plan for update");
    
    // // println!("dataframe : logical plan for UPDATE Query {:#?}", dataframe);

    // dataframe.show().await.expect("update query plan failed");

    // println!("Physical Plan for UPDATE Query {:#?}", result);

    // let updated_dataframe = ctx.sql(
    //     "SELECT * from orders WHERE customer_id=1;"
    // ).await.expect("Failed to create SELECT Query Plan");

    // updated_dataframe.show().await.expect("Failed to update");

    // ctx.sql(
    //     "INSERT INTO orders (id, customer_id, product_id, date, amount) VALUES 
    //     (7, 1, 3, '2020-01-03', 1),
    //     (8, 2, 1, '2020-01-03', 2),
    //     (9, 2, 2, '2020-01-03', 1);",
    // )
    // .await
    // .expect("Failed to create query plan for insert")
    // .collect()
    // .await
    // .expect("Failed to insert values into table");

    // let batches = ctx
    //     .sql("select product_id, sum(amount) from orders group by product_id;")
    //     .await
    //     .expect("Failed to create plan for select")
    //     .collect()
    //     .await
    //     .expect("Failed to execute select query");

    // for batch in batches {
    //     if batch.num_rows() != 0 {
    //         let (product_ids, amounts) = (
    //             batch
    //                 .column(0)
    //                 .as_any()
    //                 .downcast_ref::<Int64Array>()
    //                 .unwrap(),
    //             batch
    //                 .column(1)
    //                 .as_any()
    //                 .downcast_ref::<Int64Array>()
    //                 .unwrap(),
    //         );
    //         for (product_id, amount) in product_ids.iter().zip(amounts) {
    //             if product_id.unwrap() == 1 {
    //                 assert_eq!(amount.unwrap(), 9)
    //             } else if product_id.unwrap() == 2 {
    //                 assert_eq!(amount.unwrap(), 2)
    //             } else if product_id.unwrap() == 3 {
    //                 assert_eq!(amount.unwrap(), 4)
    //             } else {
    //                 panic!("Unexpected product id")
    //             }
    //         }
    //     }
    // }
}

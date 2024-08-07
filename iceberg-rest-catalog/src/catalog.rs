use async_trait::async_trait;
use futures::{FutureExt, TryFutureExt};
/**
Iceberg rest catalog implementation
*/
use iceberg_rust::{
    catalog::{
        bucket::{Bucket, ObjectStoreBuilder},
        commit::CommitView,
        identifier::{self, Identifier},
        namespace::Namespace,
        tabular::Tabular,
        Catalog,
    },
    error::Error,
    materialized_view::{materialized_view_builder::STORAGE_TABLE_FLAG, MaterializedView},
    spec::{
        materialized_view_metadata::MaterializedViewMetadata,
        table_metadata::TableMetadata,
        tabular::TabularMetadata,
        view_metadata::{self, ViewMetadata},
    },
    table::Table,
    view::View,
};
use object_store::ObjectStore;
use std::{collections::HashMap, path::Path, sync::Arc};

use crate::{
    apis::{
        self,
        catalog_api_api::{self, NamespaceExistsError},
        configuration::Configuration,
    },
    models,
};

#[derive(Debug)]
pub struct RestCatalog {
    name: Option<String>,
    configuration: Configuration,
    object_store_builder: ObjectStoreBuilder,
}

impl RestCatalog {
    pub fn new(
        name: Option<&str>,
        configuration: Configuration,
        object_store_builder: ObjectStoreBuilder,
    ) -> Self {
        RestCatalog {
            name: name.map(ToString::to_string),
            configuration,
            object_store_builder,
        }
    }
}

#[async_trait]
impl Catalog for RestCatalog {
    fn database_url(&self) -> std::string::String { todo!() }
    /// Create a namespace in the catalog
    async fn create_namespace(
        &self,
        namespace: &Namespace,
        properties: Option<HashMap<String, String>>,
    ) -> Result<HashMap<String, String>, Error> {
        let response = catalog_api_api::create_namespace(
            &self.configuration,
            self.name.as_deref(),
            models::CreateNamespaceRequest {
                namespace: namespace.to_vec(),
                properties,
            },
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(response.properties.unwrap_or_default())
    }
    /// Drop a namespace in the catalog
    async fn drop_namespace(&self, namespace: &Namespace) -> Result<(), Error> {
        catalog_api_api::drop_namespace(
            &self.configuration,
            self.name.as_deref(),
            &namespace.url_encode(),
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(())
    }
    /// Load the namespace properties from the catalog
    async fn load_namespace(
        &self,
        namespace: &Namespace,
    ) -> Result<HashMap<String, String>, Error> {
        let response = catalog_api_api::load_namespace_metadata(
            &self.configuration,
            self.name.as_deref(),
            &namespace.url_encode(),
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(response.properties.unwrap_or_default())
    }
    /// Update the namespace properties in the catalog
    async fn update_namespace(
        &self,
        namespace: &Namespace,
        updates: Option<HashMap<String, String>>,
        removals: Option<Vec<String>>,
    ) -> Result<(), Error> {
        catalog_api_api::update_properties(
            &self.configuration,
            self.name.as_deref(),
            &namespace.url_encode(),
            models::UpdateNamespacePropertiesRequest { updates, removals },
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(())
    }
    /// Check if a namespace exists
    async fn namespace_exists(&self, namespace: &Namespace) -> Result<bool, Error> {
        match catalog_api_api::namespace_exists(
            &self.configuration,
            self.name.as_deref(),
            &namespace.url_encode(),
        )
        .await
        {
            Ok(()) => Ok(true),
            Err(err) => {
                if let apis::Error::ResponseError(err) = err {
                    if let Some(NamespaceExistsError::Status404(_)) = err.entity {
                        Ok(false)
                    } else {
                        Err(apis::Error::ResponseError(err).into())
                    }
                } else {
                    Err(err.into())
                }
            }
        }
    }
    /// Lists all tables in the given namespace.
    async fn list_tabulars(&self, namespace: &Namespace) -> Result<Vec<Identifier>, Error> {
        let tables = catalog_api_api::list_tables(
            &self.configuration,
            self.name.as_deref(),
            &namespace.to_string(),
            None,
            None,
        )
        .await
        .map_err(Into::<Error>::into)?;
        let tables = tables.identifiers.unwrap_or(Vec::new()).into_iter();
        let views = catalog_api_api::list_views(
            &self.configuration,
            self.name.as_deref(),
            &namespace.to_string(),
            None,
            None,
        )
        .await
        .map_err(Into::<Error>::into)?;
        Ok(views
            .identifiers
            .unwrap_or(Vec::new())
            .into_iter()
            .chain(tables)
            .collect())
    }
    /// Lists all namespaces in the catalog.
    async fn list_namespaces(&self, parent: Option<&str>) -> Result<Vec<Namespace>, Error> {
        let namespaces = catalog_api_api::list_namespaces(
            &self.configuration,
            self.name.as_deref(),
            None,
            None,
            parent,
        )
        .await
        .map_err(Into::<Error>::into)?;
        namespaces
            .namespaces
            .ok_or(Error::NotFound("Namespaces".to_string(), "".to_owned()))?
            .into_iter()
            .map(|x| Namespace::try_new(&x))
            .collect()
    }
    /// Check if a table exists
    async fn tabular_exists(&self, identifier: &Identifier) -> Result<bool, Error> {
        catalog_api_api::view_exists(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .or_else(|_| async move {
            catalog_api_api::table_exists(
                &self.configuration,
                self.name.as_deref(),
                &identifier.namespace().to_string(),
                identifier.name(),
            )
            .await
        })
        .await
        .map(|_| true)
        .map_err(Into::<Error>::into)
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_table(&self, identifier: &Identifier) -> Result<(), Error> {
        catalog_api_api::drop_table(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            None,
        )
        .await
        .map_err(Into::<Error>::into)
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_view(&self, identifier: &Identifier) -> Result<(), Error> {
        catalog_api_api::drop_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .await
        .map_err(Into::<Error>::into)
    }
    /// Drop a table and delete all data and metadata files.
    async fn drop_materialized_view(&self, identifier: &Identifier) -> Result<(), Error> {
        catalog_api_api::drop_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .await
        .map_err(Into::<Error>::into)
    }
    /// Load a table.
    async fn load_tabular(self: Arc<Self>, identifier: &Identifier) -> Result<Tabular, Error> {
        // Load View/Matview metadata, is loaded as tabular to enable both possibilities. Must not be table metadata
        let tabular_metadata = catalog_api_api::load_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
        )
        .await
        .map(|x| x.metadata)
        .map_err(Into::<Error>::into)?;
        let view_metadata = match *tabular_metadata {
            TabularMetadata::View(view) => Ok(Tabular::View(
                View::new(identifier.clone(), self.clone(), view).await?,
            )),
            TabularMetadata::MaterializedView(matview) => Ok(Tabular::MaterializedView(
                MaterializedView::new(identifier.clone(), self.clone(), matview).await?,
            )),
            TabularMetadata::Table(_) => Err(Error::InvalidFormat(
                "Entity returned from load_view cannot be a table.".to_owned(),
            )),
        };

        if let Ok(view_metadata) = view_metadata {
            Ok(view_metadata)
        } else {
            let table_metadata = catalog_api_api::load_table(
                &self.configuration,
                self.name.as_deref(),
                &identifier.namespace().to_string(),
                identifier.name(),
                None,
                None,
            )
            .await
            .map(|x| x.metadata)
            .map_err(Into::<Error>::into)?;

            Ok(Tabular::Table(
                Table::new(identifier.clone(), self.clone(), *table_metadata).await?,
            ))
        }
    }
    /// Register a table with the catalog if it doesn't exist.
    async fn create_table(
        self: Arc<Self>,
        identifier: Identifier,
        metadata: TableMetadata,
    ) -> Result<Table, Error> {
        let mut request = models::CreateTableRequest::new(
            identifier.name().to_owned(),
            metadata.current_schema(None)?.clone(),
        );
        request.partition_spec = Some(Box::new(metadata.default_partition_spec()?.clone()));
        request.location = Some(metadata.location.clone());
        request.write_order = metadata
            .sort_orders
            .get(&metadata.default_sort_order_id)
            .cloned()
            .map(Box::new);
        request.properties = Some(metadata.properties);
        catalog_api_api::create_table(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            request,
            None,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            async move { Table::new(identifier.clone(), clone, *response.metadata).await }
        })
        .await
    }
    /// Update a table by atomically changing the pointer to the metadata file
    async fn update_table(
        self: Arc<Self>,
        commit: iceberg_rust::catalog::commit::CommitTable,
    ) -> Result<Table, Error> {
        let identifier = commit.identifier.clone();
        catalog_api_api::update_table(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            let identifier = identifier.clone();
            async move { Table::new(identifier, clone, *response.metadata).await }
        })
        .await
    }
    async fn create_view(
        self: Arc<Self>,
        identifier: Identifier,
        metadata: ViewMetadata,
    ) -> Result<View, Error> {
        let mut request = models::CreateViewRequest::new(
            identifier.name().to_owned(),
            metadata.current_schema(None)?.clone(),
            metadata.current_version(None)?.clone(),
            metadata.properties,
        );
        request.location = Some(metadata.location);
        catalog_api_api::create_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            request,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            async move {
                if let TabularMetadata::View(metadata) = *response.metadata {
                    View::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create view didn't return view metadata.".to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn update_view(self: Arc<Self>, commit: CommitView) -> Result<View, Error> {
        let identifier = commit.identifier.clone();
        catalog_api_api::replace_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            let identifier = identifier.clone();
            async move {
                if let TabularMetadata::View(metadata) = *response.metadata {
                    View::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create view didn't return view metadata.".to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn create_materialized_view(
        self: Arc<Self>,
        identifier: Identifier,
        metadata: MaterializedViewMetadata,
    ) -> Result<MaterializedView, Error> {
        let mut request = models::CreateViewRequest::new(
            identifier.name().to_owned(),
            metadata.current_schema(None)?.clone(),
            metadata.current_version(None)?.clone(),
            metadata.properties,
        );
        request.location = Some(metadata.location);
        catalog_api_api::create_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            request,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            async move {
                if let TabularMetadata::MaterializedView(metadata) = *response.metadata {
                    MaterializedView::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create materialzied view didn't return materialized view metadata."
                            .to_owned(),
                    ))
                }
            }
        })
        .await
    }
    async fn update_materialized_view(
        self: Arc<Self>,
        commit: CommitView,
    ) -> Result<MaterializedView, Error> {
        let identifier = commit.identifier.clone();
        catalog_api_api::replace_view(
            &self.configuration,
            self.name.as_deref(),
            &identifier.namespace().to_string(),
            identifier.name(),
            commit,
        )
        .map_err(Into::<Error>::into)
        .and_then(|response| {
            let clone = self.clone();
            let identifier = identifier.clone();
            async move {
                if let TabularMetadata::MaterializedView(metadata) = *response.metadata {
                    MaterializedView::new(identifier.clone(), clone, metadata).await
                } else {
                    Err(Error::InvalidFormat(
                        "Create materialzied view didn't return materialized view metadata."
                            .to_owned(),
                    ))
                }
            }
        })
        .await
    }
    /// Return an object store for the desired bucket
    fn object_store(&self, bucket: Bucket) -> Arc<dyn ObjectStore> {
        self.object_store_builder.build(bucket).unwrap()
    }
}

#[cfg(test)]
pub mod tests {
    use iceberg_rust::{
        catalog::{
            bucket::ObjectStoreBuilder, identifier::Identifier, namespace::Namespace, Catalog,
        },
        spec::{
            schema::Schema,
            types::{PrimitiveType, StructField, StructType, Type},
        },
        table::table_builder::TableBuilder,
    };
    use object_store::{memory::InMemory, ObjectStore};
    use std::{convert::TryFrom, sync::Arc, time::Duration};
    use testcontainers::{core::WaitFor, runners::AsyncRunner, GenericImage};

    use crate::{apis::configuration::Configuration, catalog::RestCatalog};

    fn configuration() -> Configuration {
        Configuration {
            base_path: "http://localhost:8181".to_string(),
            user_agent: None,
            client: reqwest::Client::new().into(),
            basic_auth: None,
            oauth_access_token: None,
            bearer_access_token: None,
            api_key: None,
        }
    }
    #[tokio::test]
    async fn test_create_update_drop_table() {
        let container = GenericImage::new("tabulario/iceberg-rest", "latest")
            .with_wait_for(WaitFor::StdOutMessage {
                message: "INFO  [org.eclipse.jetty.server.Server] - Started ".to_owned(),
            })
            .with_env_var("CATALOG_WAREHOUSE", "/tmp/warehouse")
            .pull_image()
            .await;

        let _node = container.with_mapped_port((8181, 8181)).start().await;

        let object_store = ObjectStoreBuilder::Memory(Arc::new(InMemory::new()));
        let catalog = Arc::new(RestCatalog::new(None, configuration(), object_store));

        catalog
            .create_namespace(&Namespace::try_new(&["public".to_owned()]).unwrap(), None)
            .await
            .expect("Failed to create namespace");

        let identifier = Identifier::parse("public.test").unwrap();

        let schema = Schema::builder()
            .with_schema_id(1)
            .with_identifier_field_ids(vec![1, 2])
            .with_fields(
                StructType::builder()
                    .with_struct_field(StructField {
                        id: 1,
                        name: "one".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::String),
                        doc: None,
                    })
                    .with_struct_field(StructField {
                        id: 2,
                        name: "two".to_string(),
                        required: true,
                        field_type: Type::Primitive(PrimitiveType::String),
                        doc: None,
                    })
                    .build()
                    .unwrap(),
            )
            .build()
            .unwrap();

        let mut builder = TableBuilder::new(&identifier, catalog.clone())
            .expect("Failed to create table builder.");
        builder
            .location("/tmp/warehouse/test")
            .with_schema((1, schema))
            .current_schema_id(1);
        let mut table = builder.build().await.expect("Failed to create table.");

        let tables = catalog
            .clone()
            .list_tabulars(
                &Namespace::try_new(&["public".to_owned()]).expect("Failed to create namespace"),
            )
            .await
            .expect("Failed to list Tables");
        assert_eq!(tables[0].to_string(), "public.test".to_owned());

        let namespaces = catalog
            .clone()
            .list_namespaces(None)
            .await
            .expect("Failed to list namespaces");
        assert_eq!(namespaces[0].to_string(), "public");

        let transaction = table.new_transaction(None);
        transaction.commit().await.expect("Transaction failed.");

        catalog
            .drop_table(&identifier)
            .await
            .expect("Failed to drop table.");
    }
}

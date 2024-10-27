/*!
Defining the [ViewBuilder] struct for creating catalog views and starting create/replace transactions
*/

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use crate::catalog::identifier::Identifier;
use crate::error::Error;
use iceberg_rust_spec::spec::schema::Schema;
use iceberg_rust_spec::spec::view_metadata::{
    VersionBuilder, ViewMetadataBuilder, ViewProperties, ViewRepresentation, REF_PREFIX,
};

use super::Catalog;
use super::View;

///Builder pattern to create a view
pub struct ViewBuilder {
    identifier: Identifier,
    catalog: Arc<dyn Catalog>,
    metadata: ViewMetadataBuilder,
}

impl Deref for ViewBuilder {
    type Target = ViewMetadataBuilder;
    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

impl DerefMut for ViewBuilder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.metadata
    }
}

impl ViewBuilder {
    /// Creates a new [TableBuilder] to create a Metastore view with some default metadata entries already set.
    pub fn new(
        sql: impl ToString,
        identifier: impl ToString,
        schema: Schema,
        catalog: Arc<dyn Catalog>,
    ) -> Result<Self, Error> {
        let identifier = Identifier::parse(&identifier.to_string(), None)?;
        let mut builder = ViewMetadataBuilder::default();
        builder
            .with_schema((1, schema))
            .with_version((
                1,
                VersionBuilder::default()
                    .version_id(1)
                    .with_representation(ViewRepresentation::Sql {
                        sql: sql.to_string(),
                        dialect: "ANSI".to_string(),
                    })
                    .schema_id(1)
                    .build()?,
            ))
            .current_version_id(1)
            .properties(ViewProperties {
                storage_table: None,
                other: HashMap::from_iter(vec![(REF_PREFIX.to_string() + "main", 1.to_string())]),
            });
        Ok(ViewBuilder {
            metadata: builder,
            identifier,
            catalog,
        })
    }
    /// Building a table writes the metadata file and commits the table to either the metastore or the filesystem
    pub async fn build(self) -> Result<View, Error> {
        let metadata = self.metadata.build()?;
        self.catalog.create_view(self.identifier, metadata).await
    }
}

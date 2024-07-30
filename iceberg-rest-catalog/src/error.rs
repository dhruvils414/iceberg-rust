use iceberg_rust::error::IcebergError;

use crate::apis::{self, catalog_api_api::CreateNamespaceError};

/**
Error conversion
*/

impl<T> Into<IcebergError> for apis::Error<T> {
    fn into(self) -> IcebergError {
        match self {
            apis::Error::Reqwest(err) => IcebergError::InvalidFormat(err.to_string()),
            apis::Error::ReqwestMiddleware(err) => IcebergError::InvalidFormat(err.to_string()),
            apis::Error::Serde(err) => IcebergError::JSONSerde(err),
            apis::Error::Io(err) => IcebergError::IO(err),
            apis::Error::ResponseError(err) => IcebergError::InvalidFormat(format!(
                "Response status: {}, Response content: {}",
                err.status, err.content
            )),
        }
    }
}

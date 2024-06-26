/*
 * Apache Iceberg REST Catalog API
 *
 * Defines the specification for the first version of the REST Catalog API. Implementations should ideally support both Iceberg table specs v1 and v2, with priority given to v2.
 *
 * The version of the OpenAPI document: 0.0.1
 * 
 * Generated by: https://openapi-generator.tech
 */

use crate::models;

/// TokenType : Token type identifier, from RFC 8693 Section 3  See https://datatracker.ietf.org/doc/html/rfc8693#section-3
/// Token type identifier, from RFC 8693 Section 3  See https://datatracker.ietf.org/doc/html/rfc8693#section-3
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
pub enum TokenType {
    #[serde(rename = "urn:ietf:params:oauth:token-type:access_token")]
    AccessToken,
    #[serde(rename = "urn:ietf:params:oauth:token-type:refresh_token")]
    RefreshToken,
    #[serde(rename = "urn:ietf:params:oauth:token-type:id_token")]
    IdToken,
    #[serde(rename = "urn:ietf:params:oauth:token-type:saml1")]
    Saml1,
    #[serde(rename = "urn:ietf:params:oauth:token-type:saml2")]
    Saml2,
    #[serde(rename = "urn:ietf:params:oauth:token-type:jwt")]
    Jwt,

}

impl ToString for TokenType {
    fn to_string(&self) -> String {
        match self {
            Self::AccessToken => String::from("urn:ietf:params:oauth:token-type:access_token"),
            Self::RefreshToken => String::from("urn:ietf:params:oauth:token-type:refresh_token"),
            Self::IdToken => String::from("urn:ietf:params:oauth:token-type:id_token"),
            Self::Saml1 => String::from("urn:ietf:params:oauth:token-type:saml1"),
            Self::Saml2 => String::from("urn:ietf:params:oauth:token-type:saml2"),
            Self::Jwt => String::from("urn:ietf:params:oauth:token-type:jwt"),
        }
    }
}

impl Default for TokenType {
    fn default() -> TokenType {
        Self::AccessToken
    }
}


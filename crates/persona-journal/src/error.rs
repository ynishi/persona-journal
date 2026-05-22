use thiserror::Error;

use persona_journal_core::CoreError;

#[derive(Debug, Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("toml decode: {0}")]
    TomlDe(#[from] toml::de::Error),
    #[error("toml encode: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unknown kind: {0}")]
    UnknownKind(String),
    #[error("entry not found: {0}")]
    EntryNotFound(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("entry already exists: {0}")]
    AlreadyExists(String),
}

impl From<CoreError> for Error {
    fn from(e: CoreError) -> Self {
        match e {
            CoreError::Io(e) => Error::Io(e),
            CoreError::TomlDe(e) => Error::TomlDe(e),
            CoreError::TomlSer(e) => Error::TomlSer(e),
            CoreError::Json(e) => Error::Json(e),
            CoreError::UnknownKind(s) => Error::UnknownKind(s),
            CoreError::EntryNotFound(s) => Error::EntryNotFound(s),
            CoreError::Invalid(s) => Error::Invalid(s),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

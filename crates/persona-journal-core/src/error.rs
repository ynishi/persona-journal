use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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
}

pub type Result<T> = std::result::Result<T, CoreError>;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("core storage I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("core SQLite error: {source}")]
    Sqlite {
        #[from]
        source: rusqlite::Error,
    },
    #[error("core schema migration error: {source}")]
    Migration {
        #[source]
        source: rusqlite::Error,
    },
    #[error("core schema version is invalid: {value}")]
    InvalidSchemaVersion { value: String },
    #[error("core schema version {version} is newer than this binary supports")]
    UnsupportedSchemaVersion { version: u32 },
    #[error("core serialization error: {source}")]
    Serialization {
        #[from]
        source: serde_json::Error,
    },
}

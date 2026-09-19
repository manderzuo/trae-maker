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
    #[error("quota amount must be positive")]
    InvalidQuotaAmount,
    #[error("actual quota amount cannot exceed the reserved amount")]
    ActualAmountExceedsReservation,
    #[error("quota reservation {reservation_id} was not found")]
    ReservationNotFound { reservation_id: String },
    #[error("quota reservation request id conflict: {request_id}")]
    ReservationRequestConflict { request_id: String },
    #[error("quota adjustment would overdraw the available balance")]
    QuotaOverdrawn,
}

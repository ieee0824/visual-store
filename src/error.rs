use serde::Serialize;
use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl Error {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: matches!(code, "E_BUSY" | "E_SOURCE_CHANGED"),
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self.code {
            "E_INVALID_ARGUMENT" | "E_INVALID_IMAGE" | "E_LIMIT_EXCEEDED" | "E_INVALID_CURSOR" => 2,
            "E_NOT_FOUND"
            | "E_STORE_NOT_INITIALIZED"
            | "E_UNSUPPORTED_IMAGE"
            | "E_UNSUPPORTED_METADATA"
            | "E_SOURCE_NOT_RETAINED" => 3,
            "E_CONFLICT"
            | "E_BUSY"
            | "E_OUTPUT_EXISTS"
            | "E_SOURCE_CHANGED"
            | "E_MIGRATION_INCOMPLETE" => 4,
            "E_INTEGRITY" | "E_SCHEMA_VERSION" | "E_STORE_MISMATCH" => 5,
            "E_CODEC_FAILURE" => 6,
            _ => 6,
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        let code = match e.raw_os_error() {
            Some(libc::ENOSPC) | Some(libc::EDQUOT) => "E_DISK_FULL",
            Some(libc::EACCES) | Some(libc::EPERM) => "E_PERMISSION",
            Some(libc::ELOOP) => "E_INTEGRITY",
            _ => "E_IO",
        };
        Self::new(
            code,
            format!("Filesystem operation failed ({:?}).", e.kind()),
        )
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        use rusqlite::ErrorCode::*;
        let code = match &e {
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::Utf8Error(..)
            | rusqlite::Error::QueryReturnedNoRows => "E_INTEGRITY",
            _ => match e.sqlite_error_code() {
                Some(DatabaseBusy | DatabaseLocked) => "E_BUSY",
                Some(DiskFull) => "E_DISK_FULL",
                Some(PermissionDenied | ReadOnly) => "E_PERMISSION",
                Some(DatabaseCorrupt | NotADatabase | ConstraintViolation | SchemaChanged) => {
                    "E_INTEGRITY"
                }
                _ => "E_IO",
            },
        };
        Self::new(code, "SQLite operation failed.")
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new("E_INTEGRITY", "Invalid stored JSON.")
    }
}

pub fn invalid(message: &str) -> Error {
    Error::new("E_INVALID_ARGUMENT", message)
}
pub fn integrity(message: &str) -> Error {
    Error::new("E_INTEGRITY", message)
}

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    NotInstalled,
    AuthRequired,
    KeyringUnavailable,
    NotEntitled,
    CapacityRefused,
    AllocationTimeout,
    SshBusy,
    PortInUse,
    PortReserved,
    SessionGone,
    SessionLimitReached,
    ConnectionLost,
    ExecTimeout,
    OrphanSuspected,
    Cancelled,
    Parse,
    Http,
    Io,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub context: Map<String, Value>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            context: Map::new(),
        }
    }

    pub fn invalid(field: &str, message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message).with("field", field)
    }

    pub fn with(mut self, key: &str, val: impl Into<Value>) -> Self {
        self.context.insert(key.to_owned(), val.into());
        self
    }

    pub fn with_kind(mut self, kind: ErrorKind) -> Self {
        self.kind = kind;
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{:?}] {}", self.kind, self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::Io, e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::new(ErrorKind::Parse, e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

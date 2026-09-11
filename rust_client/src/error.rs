//! Errors from the child process, protocol, and backend.

use std::error::Error as StdError;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

use crate::BackendError;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Serialization(serde_json::Error),
    Protocol(String),
    Backend(BackendError),
    BackendExited(Option<i32>),
    BackendFailed(ExitStatus),
    NonUtf8Path(PathBuf),
    RequestIdExhausted,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "backend I/O failed: {error}"),
            Self::Serialization(error) => write!(formatter, "invalid bridge value: {error}"),
            Self::Protocol(message) => write!(formatter, "bridge protocol error: {message}"),
            Self::Backend(error) => {
                write!(
                    formatter,
                    "backend rejected request ({}): {}",
                    error.code, error.message
                )
            }
            Self::BackendExited(code) => write!(formatter, "backend exited unexpectedly: {code:?}"),
            Self::BackendFailed(status) => write!(formatter, "backend exited with {status}"),
            Self::NonUtf8Path(path) => {
                write!(formatter, "path is not valid UTF-8: {}", path.display())
            }
            Self::RequestIdExhausted => write!(formatter, "bridge request IDs are exhausted"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Serialization(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

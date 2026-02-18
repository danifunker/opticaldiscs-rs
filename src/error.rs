//! Unified error type for the opticaldiscs library.

use thiserror::Error;

/// All errors that can be produced by this library.
#[derive(Debug, Error)]
pub enum OpticaldiscsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),

    #[error("Unsupported filesystem")]
    UnsupportedFilesystem,

    #[error("Entry not found: {0}")]
    NotFound(String),

    #[error("Not a directory: {0}")]
    NotADirectory(String),

    #[error("Invalid data: {0}")]
    InvalidData(String),

    #[error("CHD error: {0}")]
    Chd(String),

    #[error("CUE error: {0}")]
    Cue(String),

    #[error("No data track found")]
    NoDataTrack,
}

pub type Result<T> = std::result::Result<T, OpticaldiscsError>;

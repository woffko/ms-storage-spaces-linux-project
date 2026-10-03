use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The volume holds something this reader does not understand as ReFS.
    #[error("invalid ReFS structure: {0}")]
    Format(String),
    /// Valid ReFS that this reader does not support (yet).
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("not found: {0}")]
    NotFound(String),
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! format_err {
    ($($arg:tt)*) => { $crate::error::Error::Format(format!($($arg)*)) };
}
pub(crate) use format_err;

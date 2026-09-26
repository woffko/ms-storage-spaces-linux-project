use std::io;

/// Errors produced while reading a pool.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// On-disk data does not match the expected format.
    #[error("invalid on-disk data: {0}")]
    Format(String),
    /// The data is valid but uses a feature this crate cannot handle yet.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Pool members are missing or inconsistent.
    #[error("pool error: {0}")]
    Pool(String),
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! format_err {
    ($($arg:tt)*) => { $crate::error::Error::Format(format!($($arg)*)) };
}
pub(crate) use format_err;

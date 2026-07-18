//! Errors surfaced by the remote cache and its lock backends.

use std::fmt;

/// A failure while parsing a cache URL, building/extracting an archive, talking
/// to the OCI registry, or acquiring a lock.
#[derive(Debug)]
pub enum CacheError {
    /// An I/O failure (file open, read, write, rename).
    Io(std::io::Error),
    /// A zip archive read or write failure.
    Zip(String),
    /// A malformed cache or lock URL.
    Url(String),
    /// A failure from the OCI store or the vk lock backend.
    Ocicas(ocicas::Error),
    /// A backend requested by the URL scheme that this build does not support.
    Unsupported(String),
    /// Any other message-only failure.
    Msg(String),
}

impl CacheError {
    pub fn msg(s: impl Into<String>) -> CacheError {
        CacheError::Msg(s.into())
    }
    pub fn url(s: impl Into<String>) -> CacheError {
        CacheError::Url(s.into())
    }
    pub fn unsupported(s: impl Into<String>) -> CacheError {
        CacheError::Unsupported(s.into())
    }
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "{e}"),
            CacheError::Zip(s) => write!(f, "{s}"),
            CacheError::Url(s) => write!(f, "{s}"),
            CacheError::Ocicas(e) => write!(f, "{e}"),
            CacheError::Unsupported(s) => write!(f, "{s}"),
            CacheError::Msg(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<std::io::Error> for CacheError {
    fn from(e: std::io::Error) -> Self {
        CacheError::Io(e)
    }
}

impl From<zip::result::ZipError> for CacheError {
    fn from(e: zip::result::ZipError) -> Self {
        CacheError::Zip(e.to_string())
    }
}

impl From<ocicas::Error> for CacheError {
    fn from(e: ocicas::Error) -> Self {
        CacheError::Ocicas(e)
    }
}

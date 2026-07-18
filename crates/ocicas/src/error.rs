//! The crate error type. Few variants, so `Display`/`From` are hand-written
//! rather than pulling in a derive macro.

use std::fmt;

/// Result specialized to this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// A failure building, validating, or transferring a content-addressed file set.
#[derive(Debug)]
pub enum Error {
    /// A filesystem or I/O failure (zstd stream errors surface here too — the
    /// `zstd` crate reports them as `std::io::Error`).
    Io(std::io::Error),
    /// A JSON (de)serialization failure of the index.
    Json(serde_json::Error),
    /// A format, validation, or integrity failure — the message carries the
    /// specific reason (unsupported version, unsafe path, digest mismatch, …).
    Format(String),
    /// A network-level failure reaching the registry (connect refused/timeout,
    /// stalled transfer). Distinguished from [`Error::Format`] so callers can
    /// report an unreachable cache separately from a cache miss or bad content.
    Network(String),
}

impl Error {
    /// Build a [`Error::Format`] from a message.
    pub(crate) fn format(msg: impl Into<String>) -> Self {
        Error::Format(msg.into())
    }

    /// Build a [`Error::Network`] from a message.
    pub(crate) fn network(msg: impl Into<String>) -> Self {
        Error::Network(msg.into())
    }

    /// Whether this is a network-level failure reaching the registry, as opposed
    /// to a miss or a content/format error.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Error::Network(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "ocicas: {e}"),
            Error::Json(e) => write!(f, "ocicas: {e}"),
            Error::Format(m) => write!(f, "ocicas: {m}"),
            Error::Network(m) => write!(f, "ocicas: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Json(e) => Some(e),
            Error::Format(_) => None,
            Error::Network(_) => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_unreachable_only_for_network() {
        assert!(Error::network("connect refused").is_unreachable());
        assert!(!Error::format("digest mismatch").is_unreachable());
        assert!(!Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)).is_unreachable());
    }
}

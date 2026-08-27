//! The crate error type. Few variants, so `Display`/`From` are hand-written
//! rather than pulling in a derive macro.

use std::fmt;

/// How deep [`with_causes`] walks. Deeper than any chain observed from a
/// `reqwest` failure (three links); the rest is headroom.
const MAX_CAUSES: usize = 8;

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

/// Render `e` with its source chain appended (`: cause: cause`). `reqwest`'s
/// own `Display` stops at "error sending request for url (…)" and keeps the
/// reason — TLS trust, connection refused, DNS — further down the chain, so
/// without this a rejected certificate reads exactly like an unreachable host.
///
/// Bounded at [`MAX_CAUSES`], since the argument is any error and a chain that
/// cycles would otherwise never end.
pub(crate) fn with_causes(prefix: &str, e: &dyn std::error::Error) -> String {
    let mut msg = format!("{prefix}: {e}");
    for cause in std::iter::successors(e.source(), |c| c.source()).take(MAX_CAUSES) {
        let text = cause.to_string();
        // Skip a cause the message already ends with — the shape
        // `io::Error::other(inner)` produces — to keep the line readable.
        if !msg.ends_with(&text) {
            msg.push_str(": ");
            msg.push_str(&text);
        }
    }
    msg
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

    /// A minimal error with a settable source, to build a chain by hand.
    #[derive(Debug)]
    struct Link {
        text: &'static str,
        source: Option<Box<Link>>,
    }

    impl fmt::Display for Link {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.text)
        }
    }

    impl std::error::Error for Link {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.source
                .as_ref()
                .map(|s| s.as_ref() as &(dyn std::error::Error + 'static))
        }
    }

    fn link(text: &'static str, source: Option<Link>) -> Link {
        Link {
            text,
            source: source.map(Box::new),
        }
    }

    // The whole point of the helper: the reason a request failed sits below the
    // outer message, which is where `Display` alone stops.
    #[test]
    fn causes_are_appended() {
        let chain = link("outer", Some(link("middle", Some(link("inner", None)))));
        assert_eq!(with_causes("http", &chain), "http: outer: middle: inner");
    }

    // A cause the message already ends with — what `io::Error::other(inner)`
    // renders as — is not repeated.
    #[test]
    fn a_restated_cause_is_not_repeated() {
        let chain = link("boom", Some(link("boom", None)));
        assert_eq!(with_causes("http", &chain), "http: boom");
    }

    #[test]
    fn is_unreachable_only_for_network() {
        assert!(Error::network("connect refused").is_unreachable());
        assert!(!Error::format("digest mismatch").is_unreachable());
        assert!(!Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)).is_unreachable());
    }
}

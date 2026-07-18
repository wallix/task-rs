//! A small URL parser for cache and lock URLs. The remote cache accepts a
//! handful of schemes (`file`, `oci`, `vk`, `vks`, `redis`) whose only
//! structured parts are the userinfo, host, path and query. The standard
//! library has no URL type, so this parses exactly those parts with the same
//! splitting rules the reference implementation relied on.

use std::collections::BTreeMap;

/// The parsed pieces of a cache or lock URL.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheUri {
    pub scheme: String,
    /// Userinfo username (empty when absent).
    pub username: String,
    /// Userinfo password (`None` when the userinfo had no `:`).
    pub password: Option<String>,
    /// Host (with optional `:port`).
    pub host: String,
    /// Path, including the leading `/` when present.
    pub path: String,
    /// Raw query string (without the leading `?`).
    pub raw_query: String,
}

impl CacheUri {
    /// Parse `raw` into its scheme, authority, path and query. Returns `None`
    /// when there is no `scheme://` prefix.
    pub fn parse(raw: &str) -> Option<CacheUri> {
        let (scheme, rest) = raw.split_once("://")?;
        let mut uri = CacheUri {
            scheme: scheme.to_string(),
            ..CacheUri::default()
        };

        // Split off the query, then the fragment is not used by any backend.
        let (authority_path, query) = match rest.split_once('?') {
            Some((left, q)) => (left, q),
            None => (rest, ""),
        };
        uri.raw_query = query.to_string();

        // authority ends at the first '/', which begins the path.
        let (authority, path) = match authority_path.find('/') {
            Some(i) => authority_path.split_at(i),
            None => (authority_path, ""),
        };
        uri.path = path.to_string();

        // userinfo@host
        let host = match authority.rsplit_once('@') {
            Some((userinfo, host)) => {
                match userinfo.split_once(':') {
                    Some((u, p)) => {
                        uri.username = u.to_string();
                        uri.password = Some(p.to_string());
                    }
                    None => uri.username = userinfo.to_string(),
                }
                host
            }
            None => authority,
        };
        uri.host = host.to_string();
        Some(uri)
    }

    /// Decode the query into key/value pairs, percent-decoding both sides.
    pub fn query(&self) -> BTreeMap<String, String> {
        let mut map = BTreeMap::new();
        for pair in self.raw_query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            map.insert(percent_decode(k), percent_decode(v));
        }
        map
    }

    /// The URL with any password replaced by `xxxxx`, for error messages.
    pub fn redacted(&self) -> String {
        let mut out = format!("{}://", self.scheme);
        if !self.username.is_empty() || self.password.is_some() {
            out.push_str(&self.username);
            if self.password.is_some() {
                out.push_str(":xxxxx");
            }
            out.push('@');
        }
        out.push_str(&self.host);
        out.push_str(&self.path);
        if !self.raw_query.is_empty() {
            out.push('?');
            out.push_str(&self.raw_query);
        }
        out
    }
}

/// Percent-decode a query component, treating `+` as a literal (query values in
/// these URLs are file paths and flags, not form-encoded).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes.get(i) {
            Some(b'%') => {
                let hi = bytes.get(i.saturating_add(1)).copied();
                let lo = bytes.get(i.saturating_add(2)).copied();
                match (hi.and_then(hex_val), lo.and_then(hex_val)) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i = i.saturating_add(3);
                    }
                    _ => {
                        out.push(b'%');
                        i = i.saturating_add(1);
                    }
                }
            }
            Some(&b) => {
                out.push(b);
                i = i.saturating_add(1);
            }
            None => break,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    char::from(b).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_oci_with_userinfo_and_query() {
        let u = CacheUri::parse(
            "oci://ci:secret@10.10.140.49/task-cache:tag?ca=/etc/ca.crt&cas=/var/cache/x",
        )
        .unwrap();
        assert_eq!(u.scheme, "oci");
        assert_eq!(u.username, "ci");
        assert_eq!(u.password.as_deref(), Some("secret"));
        assert_eq!(u.host, "10.10.140.49");
        assert_eq!(u.path, "/task-cache:tag");
        let q = u.query();
        assert_eq!(q.get("ca").map(String::as_str), Some("/etc/ca.crt"));
        assert_eq!(q.get("cas").map(String::as_str), Some("/var/cache/x"));
    }

    #[test]
    fn parses_file_url_path() {
        let u = CacheUri::parse("file:///home/user/cache/build.zip").unwrap();
        assert_eq!(u.scheme, "file");
        assert_eq!(u.host, "");
        assert_eq!(u.path, "/home/user/cache/build.zip");
    }

    #[test]
    fn redacts_password() {
        let u = CacheUri::parse("oci://ci:secret@host/repo:tag").unwrap();
        assert_eq!(u.redacted(), "oci://ci:xxxxx@host/repo:tag");
    }

    #[test]
    fn no_scheme_returns_none() {
        assert!(CacheUri::parse("just-a-path").is_none());
    }
}

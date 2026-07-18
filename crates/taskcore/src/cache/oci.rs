//! The `oci://` cache transport: the entry is an ocicas artifact (content-defined
//! chunks deduplicated registry-side) rather than a zip blob. URL shape:
//!
//! ```text
//! oci://[user:password@]host/repo:tag[?ca=<file>][&cas=<dir>][&plainhttp=1]
//! ```
//!
//! The tag carries the cache key. `ca` adds a trust anchor (self-signed corp
//! registry), `cas` overrides the local chunk store (default:
//! `<user cache dir>/task/ocicas`), `plainhttp` is for local dev registries.
//! Credentials and trust can also come from the environment, keeping secrets
//! out of the Taskfile: `TASK_CACHE_OCI_USER`, `TASK_CACHE_OCI_PASSWORD`,
//! `TASK_CACHE_OCI_CA` and `TASK_CACHE_OCI_CAS_DIR`.

use std::path::PathBuf;

use ocicas::RemoteOptions;

use super::error::CacheError;
use super::url::CacheUri;

/// Annotation keys of the cache metadata on the artifact manifest — the same
/// vocabulary as the zip comment.
pub const ANN_TASK: &str = "com.wallix.task.name";
pub const ANN_SOURCES: &str = "com.wallix.task.sources";
pub const ANN_GENERATES: &str = "com.wallix.task.generates";

/// Split an `oci://` cache URL into the repository reference, the tag, and the
/// store options.
pub fn parse_oci_cache_url(u: &CacheUri) -> Result<(String, String, RemoteOptions), CacheError> {
    let last = u.path.rfind(':').ok_or_else(|| {
        CacheError::url(format!("oci cache url {:?}: missing :tag", u.redacted()))
    })?;
    let repo_path = u.path.get(..last).unwrap_or("");
    let tag = u.path.get(last.saturating_add(1)..).unwrap_or("");
    if tag.is_empty() {
        return Err(CacheError::url(format!(
            "oci cache url {:?}: empty tag",
            u.redacted()
        )));
    }
    let repo = format!("{}{}", u.host, repo_path);

    let mut opts = RemoteOptions::default();
    if !u.username.is_empty() || u.password.is_some() {
        opts.username = u.username.clone();
        opts.password = u.password.clone().unwrap_or_default();
    }
    if opts.username.is_empty() {
        opts.username = env("TASK_CACHE_OCI_USER");
        opts.password = env("TASK_CACHE_OCI_PASSWORD");
    }

    let q = u.query();
    let ca = q.get("ca").cloned().unwrap_or_default();
    let ca = if ca.is_empty() {
        env("TASK_CACHE_OCI_CA")
    } else {
        ca
    };
    opts.ca_file = (!ca.is_empty()).then(|| PathBuf::from(ca));

    opts.plain_http = q.get("plainhttp").map(String::as_str) == Some("1");

    let mut cas = q.get("cas").cloned().unwrap_or_default();
    if cas.is_empty() {
        cas = env("TASK_CACHE_OCI_CAS_DIR");
    }
    if cas.is_empty()
        && let Some(base) = user_cache_dir()
    {
        cas = base
            .join("task")
            .join("ocicas")
            .to_string_lossy()
            .into_owned();
    }
    opts.cache_dir = (!cas.is_empty()).then(|| PathBuf::from(cas));

    Ok((repo, tag.to_string(), opts))
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// Best-effort equivalent of Go's `os.UserCacheDir` on the supported platforms:
/// `$XDG_CACHE_HOME`, then `$HOME/.cache`.
fn user_cache_dir() -> Option<PathBuf> {
    if let Ok(x) = std::env::var("XDG_CACHE_HOME")
        && !x.is_empty()
    {
        return Some(PathBuf::from(x));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Result<(String, String, RemoteOptions), CacheError> {
        parse_oci_cache_url(&CacheUri::parse(raw).unwrap())
    }

    #[test]
    fn full_url() {
        let (repo, tag, opts) = parse(
            "oci://ci:secret@10.10.140.49/task-cache:build-thing-abc123?ca=/etc/ca.crt&cas=/var/cache/x",
        )
        .unwrap();
        assert_eq!(repo, "10.10.140.49/task-cache");
        assert_eq!(tag, "build-thing-abc123");
        assert_eq!(opts.username, "ci");
        assert_eq!(opts.password, "secret");
        assert_eq!(
            opts.ca_file.as_deref(),
            Some(std::path::Path::new("/etc/ca.crt"))
        );
        assert_eq!(
            opts.cache_dir.as_deref(),
            Some(std::path::Path::new("/var/cache/x"))
        );
        assert!(!opts.plain_http);
    }

    #[test]
    fn default_cas_dir() {
        // Ensure a resolvable HOME so the default path can be computed.
        // SAFETY: single-threaded test setup.
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::remove_var("TASK_CACHE_OCI_CAS_DIR");
            std::env::remove_var("XDG_CACHE_HOME");
        }
        let (_, _, opts) = parse("oci://host/repo:tag").unwrap();
        assert!(opts.cache_dir.is_some(), "expected a default chunk CAS dir");
    }

    #[test]
    fn plain_http() {
        let (_, _, opts) = parse("oci://host/repo:tag?plainhttp=1").unwrap();
        assert!(opts.plain_http);
    }

    #[test]
    fn missing_or_empty_tag_errors() {
        assert!(parse("oci://host/repo").is_err());
        assert!(parse("oci://host/repo:").is_err());
    }
}

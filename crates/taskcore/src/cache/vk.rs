//! The `cache.vk` model: one vk-registry repository serving both the cache and
//! the lock, so a Taskfile names the registry once and the `oci://` and
//! `vks://` (or `vk://`) URLs are derived here.
//!
//! ```text
//! vk: host[:port]/repo         →  url:  oci://host[:port]/repo:<namespace>-<task>-<checksum>
//!                                 lock: vks://host[:port]/repo/<namespace>
//! vk: http://host[:port]/repo  →  url:  oci://host[:port]/repo:<…>?plainhttp=1
//!                                 lock: vk://host[:port]/repo/<namespace>
//! ```
//!
//! In the lock, `<namespace>` is made a repository name component
//! ([`lock_component`]).
//!
//! `http://` is for virtkit's guest registry proxy (`http://registry.vk`);
//! `https://` explicitly selects the default HTTPS transport.
//!
//! `<task>` is the task's name in the Taskfile that defines it, with no include
//! namespace or `label`, so every include path to a task shares its entry.
//! `<checksum>` is `hash::cache_checksum`: the sources checksum, extended with
//! a digest of the task's and the include's `vars`. Nothing in the tag names
//! the defining Taskfile, so two Taskfiles defining a task of the same name
//! whose sources, commands, generates and `vars` all match share an entry;
//! only file-level `vars:`, `env:` or `dotenv:` could then tell them apart.

/// Longest tag an OCI registry accepts (`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`).
const MAX_TAG_LEN: usize = 128;
/// Room kept for the task name in a tag, so a long name leaves the namespace
/// and the checksum intact. Two tasks whose names only differ past the cap (or
/// only by a character the tag alphabet lacks) share a tag; the entry's task
/// name annotation makes the collision a verified miss, not a wrong restore.
const MAX_TASK_LEN: usize = 48;

/// Returns the TLS flag and `host[:port]/repo` after stripping an optional
/// `http://` or `https://` prefix. Other spellings stay in the host and fail
/// [`is_valid`]; only lowercase schemes match.
fn split_scheme(vk: &str) -> (bool, &str) {
    if let Some(rest) = vk.strip_prefix("http://") {
        (false, rest)
    } else {
        (true, vk.strip_prefix("https://").unwrap_or(vk))
    }
}

/// Validates a rendered `[http[s]://]host[:port]/repo`. Rejects credentials,
/// query, fragment, tag and whitespace, which would otherwise be copied
/// verbatim into the derived URLs.
pub fn is_valid(vk: &str) -> bool {
    let (_, vk) = split_scheme(vk);
    let Some((host, repo)) = vk.split_once('/') else {
        return false;
    };
    let host_ok = !host.is_empty()
        && host.split_once(':').is_none_or(|(h, p)| {
            !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
        });
    let repo_ok = repo.split('/').all(|seg| !seg.is_empty())
        && repo.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '/' | '.' | '_' | '-')
        });
    host_ok && repo_ok && !host.contains(['@', '?', '#']) && !vk.chars().any(char::is_whitespace)
}

/// The `oci://` cache URL for `task`'s entry at `checksum` under `vk`.
pub fn cache_url(vk: &str, namespace: &str, task: &str, checksum: &str) -> String {
    let (tls, vk) = split_scheme(vk);
    let plain = if tls { "" } else { "?plainhttp=1" };
    format!("oci://{vk}:{}{plain}", tag(namespace, task, checksum))
}

/// The `vks://` (`vk://` over plain HTTP) lock URL under `vk`: the namespace
/// is the key prefix, the executor appends the task name and checksum.
pub fn lock_url(vk: &str, namespace: &str) -> String {
    let (tls, vk) = split_scheme(vk);
    let scheme = if tls { "vks" } else { "vk" };
    let ns = lock_component(namespace);
    if ns.is_empty() {
        format!("{scheme}://{vk}")
    } else {
        format!("{scheme}://{vk}/{ns}")
    }
}

/// An OCI tag from the namespace, the task name and the checksum. Characters
/// outside the tag alphabet become `-`; the task name is capped so a long one
/// cannot crowd out the checksum; a tag must start with a letter, digit or `_`.
fn tag(namespace: &str, task: &str, checksum: &str) -> String {
    let mut head = sanitize(namespace);
    let task: String = sanitize(task).chars().take(MAX_TASK_LEN).collect();
    if !task.is_empty() {
        if !head.is_empty() {
            head.push('-');
        }
        head.push_str(&task);
    }
    // A tag must start with a letter, digit or `_`; decide that before the
    // cap so the prefix counts against it. (The checksum is hex, so it never
    // needs one itself.)
    if head.starts_with(['.', '-']) {
        head.insert(0, '_');
    }
    // The checksum is the key; trim the head rather than lose any of it.
    let room = MAX_TAG_LEN
        .saturating_sub(checksum.len())
        .saturating_sub(usize::from(!head.is_empty() && !checksum.is_empty()));
    let mut tag: String = head.chars().take(room).collect();
    if !checksum.is_empty() {
        if !tag.is_empty() {
            tag.push('-');
        }
        tag.push_str(checksum);
    }
    tag
}

/// Normalize `namespace` to the lock name's last repository path component.
/// vk-registry rejects fleet job locks whose paths fall outside repository
/// grammar when checking a hub token's grants.
///
/// Replace characters outside `[A-Za-z0-9._-]` with `-` and lowercase. If still
/// invalid, replace everything outside `[a-z0-9]` with `-` and trim the ends.
/// Return empty when nothing remains. Distinct namespaces can share a
/// component; that only serializes their builds, as each keeps its own tag.
fn lock_component(namespace: &str) -> String {
    // Sanitize first to preserve `.` and `_` when replacing spaces or other
    // invalid characters suffices, avoiding the all-`-` fallback.
    let ns = sanitize(namespace).to_ascii_lowercase();
    if is_component(&ns) {
        return ns;
    }
    let dashed: String = ns
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    dashed.trim_matches('-').to_string()
}

/// Whether `c` is a repository name component, OCI's
/// `[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*`. Mirrors vk-hub-proto's
/// `valid_component`, which the registry checks lock names with.
fn is_component(c: &str) -> bool {
    let alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let mut rest = c.as_bytes();
    loop {
        let run = rest.iter().take_while(|b| alnum(b)).count();
        if run == 0 {
            return false;
        }
        rest = rest.get(run..).unwrap_or_default();
        let sep = match rest {
            [] => return true,
            [b'_', b'_', ..] => 2,
            [b'.' | b'_', ..] => 1,
            [b'-', ..] => rest.iter().take_while(|&&b| b == b'-').count(),
            _ => return false,
        };
        rest = rest.get(sep..).unwrap_or_default();
    }
}

/// Map every character outside `[A-Za-z0-9._-]` to `-`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_derived_from_the_repository() {
        assert_eq!(
            cache_url(
                "reg.example:5000/task-cache",
                "gcc-13",
                "app:build",
                "abc123"
            ),
            "oci://reg.example:5000/task-cache:gcc-13-app-build-abc123"
        );
        assert_eq!(
            lock_url("reg.example:5000/task-cache", "gcc-13"),
            "vks://reg.example:5000/task-cache/gcc-13"
        );
        assert_eq!(
            lock_url("reg.example/task-cache", ""),
            "vks://reg.example/task-cache"
        );
        assert_eq!(
            cache_url("https://reg.example/task-cache", "", "build", "abc"),
            "oci://reg.example/task-cache:build-abc"
        );
        assert_eq!(
            lock_url("https://reg.example/task-cache", ""),
            "vks://reg.example/task-cache"
        );
    }

    #[test]
    fn http_selects_plain_http() {
        assert_eq!(
            cache_url("http://registry.vk/task-cache", "gcc-13", "build", "abc"),
            "oci://registry.vk/task-cache:gcc-13-build-abc?plainhttp=1"
        );
        assert_eq!(
            lock_url("http://registry.vk/task-cache", "gcc-13"),
            "vk://registry.vk/task-cache/gcc-13"
        );
    }

    #[test]
    fn only_a_bare_host_and_repository_is_valid() {
        for ok in [
            "reg.example/task-cache",
            "reg:5000/a/b",
            "127.0.0.1:1/task-cache",
            "http://registry.vk/task-cache",
            "https://reg/repo",
        ] {
            assert!(is_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "reg",
            "reg/",
            "/repo",
            "http://",
            "ftp://reg/repo",
            "http://user@reg/repo",
            "HTTP://reg/repo",
            "reg/repo:tag",
            "reg/repo?ca=x",
            "reg/repo#f",
            "user@reg/repo",
            "reg /repo",
            "reg:/repo",
            "reg:port/repo",
            "reg//repo",
            "reg/Repo",
        ] {
            assert!(!is_valid(bad), "{bad}");
        }
    }

    /// A lock name's namespace component stays in repository-name grammar, so
    /// a hub token's grants can match it; one already in it is unchanged.
    #[test]
    fn lock_namespace_is_a_repository_name_component() {
        for (ns, want) in [
            ("gcc-13", "gcc-13"),
            ("gcc_13", "gcc_13"),
            ("a.b__c", "a.b__c"),
            ("GCC_13", "gcc_13"),
            ("gcc 13", "gcc-13"),
            ("-dev", "dev"),
            ("a._b", "a--b"),
            ("a/b", "a-b"),
            ("...", ""),
            ("", ""),
            ("v1.2", "v1.2"),
            ("a-", "a"),
            ("a___b", "a---b"),
            ("gcc\u{2013}13", "gcc-13"),
            ("\u{e9}", ""),
            ("gcc_1 3", "gcc_1-3"),
        ] {
            assert_eq!(lock_component(ns), want, "{ns:?}");
            assert!(want.is_empty() || is_component(want), "{want:?}");
        }
        assert_eq!(lock_url("reg/repo", "GCC_13"), "vks://reg/repo/gcc_13");
        assert_eq!(lock_url("reg/repo", "..."), "vks://reg/repo");
    }

    #[test]
    fn tag_is_a_valid_oci_tag() {
        // No namespace: task and checksum only.
        assert_eq!(tag("", "build", "abc"), "build-abc");
        // Characters outside the alphabet, including `/` and `:`, become `-`.
        assert_eq!(tag("a/b c", "x:y", "1"), "a-b-c-x-y-1");
        // A leading `.` or `-` is not allowed to start a tag.
        assert_eq!(tag("-dev", "build", "1"), "_-dev-build-1");
        // A long task name is capped; the checksum always survives.
        let long = "t".repeat(200);
        let t = tag("ns", &long, "abc");
        assert_eq!(t, format!("ns-{}-abc", "t".repeat(MAX_TASK_LEN)));
        // A long namespace is trimmed to keep the whole tag within the limit.
        let t = tag(&"n".repeat(200), "build", &"c".repeat(32));
        assert_eq!(t.len(), MAX_TAG_LEN);
        assert!(t.ends_with(&format!("-{}", "c".repeat(32))));
        // ... including the `_` a leading `-` costs.
        let t = tag(&format!("-{}", "n".repeat(200)), "build", &"c".repeat(32));
        assert_eq!(t.len(), MAX_TAG_LEN);
        assert!(t.starts_with("_-n"));
    }
}

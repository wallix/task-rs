//! The `cache.vk` model: one vk-registry repository serving both the cache and
//! the lock, so a Taskfile names the registry once and the `oci://` and
//! `vks://` URLs are derived here.
//!
//! ```text
//! vk: host[:port]/repo      →  url:  oci://host[:port]/repo:<namespace>-<task>-<checksum>
//!                              lock: vks://host[:port]/repo/<namespace>
//! ```

/// Longest tag an OCI registry accepts (`[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`).
const MAX_TAG_LEN: usize = 128;
/// Room kept for the task name in a tag, so a long name leaves the namespace
/// and the checksum intact. Two tasks whose names only differ past the cap (or
/// only by a character the tag alphabet lacks) share a tag; the entry's task
/// name annotation makes the collision a verified miss, not a wrong restore.
const MAX_TASK_LEN: usize = 48;

/// Whether a rendered `vk` is a bare `host[:port]/repo`: no scheme, credentials,
/// query, fragment, tag or whitespace, since all of those would be copied
/// verbatim into the derived URLs.
pub fn is_valid(vk: &str) -> bool {
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
    format!("oci://{vk}:{}", tag(namespace, task, checksum))
}

/// The `vks://` lock URL under `vk`: the namespace is the key prefix, the
/// executor appends the task name and checksum.
pub fn lock_url(vk: &str, namespace: &str) -> String {
    let ns = sanitize(namespace);
    if ns.is_empty() {
        format!("vks://{vk}")
    } else {
        format!("vks://{vk}/{ns}")
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
    }

    #[test]
    fn only_a_bare_host_and_repository_is_valid() {
        for ok in [
            "reg.example/task-cache",
            "reg:5000/a/b",
            "127.0.0.1:1/task-cache",
        ] {
            assert!(is_valid(ok), "{ok}");
        }
        for bad in [
            "",
            "reg",
            "reg/",
            "/repo",
            "https://reg/repo",
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

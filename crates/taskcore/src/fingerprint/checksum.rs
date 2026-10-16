//! The fingerprint checksum, format v1.
//!
//! Every file is digested on its own: a regular file by its content, a symlink
//! by its target. The task checksum is then one hash over a versioned,
//! length-framed record of each entry (its path relative to the task
//! directory, its kind and its digest) in path order, followed by the task's
//! structurally encoded rules. Hash: 128-bit xxHash3, rendered as 32 lower-hex
//! digits. Fast and non-cryptographic — a change-detection fingerprint, not a
//! security digest. Changing anything that goes into it invalidates every
//! saved fingerprint and cache key, so bump the version tag when doing so.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use twox_hash::XxHash3_128;

use crate::filepathext;

use super::pool::{Workers, digest_all};

/// Leads every checksum, so a future format cannot collide with this one.
const VERSION: &[u8] = b"task-rs/fingerprint/v1\0";

/// The read buffer for file contents.
pub(crate) const BUF_LEN: usize = 128 * 1024;

/// A non-file input to a sources checksum. Each is hashed by its fields, so
/// no two distinct inputs encode alike; [`Datum::display`] is the text
/// `--status` shows for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Datum {
    /// A `value:` source: the text after the prefix.
    Value(String),
    /// A `sources` glob, relative to the task directory.
    SrcRule {
        group: u32,
        negate: bool,
        glob: String,
    },
    /// A `generates` glob, relative to the task directory.
    GenRule {
        group: u32,
        negate: bool,
        glob: String,
    },
    /// A raw (unrendered) command, by position.
    Cmd { index: u64, cmd: String },
}

impl Datum {
    /// `value:<v>`, `srcrule[@<group>]:[!]<glob>`, `genrule…`, `cmd[<i>]:<cmd>`.
    pub fn display(&self) -> String {
        let rule = |kind: &str, group: u32, negate: bool, glob: &str| {
            let negate = if negate { "!" } else { "" };
            if group == 0 {
                format!("{kind}:{negate}{glob}")
            } else {
                format!("{kind}@{group}:{negate}{glob}")
            }
        };
        match self {
            Datum::Value(v) => format!("value:{v}"),
            Datum::SrcRule {
                group,
                negate,
                glob,
            } => rule("srcrule", *group, *negate, glob),
            Datum::GenRule {
                group,
                negate,
                glob,
            } => rule("genrule", *group, *negate, glob),
            Datum::Cmd { index, cmd } => format!("cmd[{index}]:{cmd}"),
        }
    }

    /// A tag byte, then each field, strings length-prefixed.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let rule = |out: &mut Vec<u8>, tag: u8, group: u32, negate: bool, glob: &str| {
            out.push(tag);
            out.extend_from_slice(&group.to_le_bytes());
            out.push(u8::from(negate));
            put_bytes(out, glob.as_bytes());
        };
        match self {
            Datum::Value(v) => {
                out.push(b'V');
                put_bytes(&mut out, v.as_bytes());
            }
            Datum::SrcRule {
                group,
                negate,
                glob,
            } => rule(&mut out, b'S', *group, *negate, glob),
            Datum::GenRule {
                group,
                negate,
                glob,
            } => rule(&mut out, b'G', *group, *negate, glob),
            Datum::Cmd { index, cmd } => {
                out.push(b'C');
                out.extend_from_slice(&index.to_le_bytes());
                put_bytes(&mut out, cmd.as_bytes());
            }
        }
        out
    }
}

/// How an entry is digested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    /// A regular file, by its content.
    File,
    /// A symlink, by its target, never followed.
    Link,
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::File => b'F',
            Kind::Link => b'L',
        }
    }
}

/// A file to digest, in canonical order.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// `R` for a key relative to the task directory, `A` for the joined path
    /// when no relative one exists.
    pub tag: u8,
    /// The path the checksum records.
    pub key: String,
    /// The path to read.
    pub path: String,
}

/// Maps the glob results to entries keyed by path relative to `basedir`, sorted
/// by key. Keys are cleaned lexically, so two results can share one (`a/./x`
/// and `a/x`, or `link/../x` and `x`, which may be different files): each is
/// still hashed, ordered by its joined path, rather than merged.
pub(crate) fn entries(basedir: &str, files: &[String]) -> Vec<Entry> {
    let mut out: Vec<Entry> = files
        .iter()
        .map(|f| match filepathext::rel_str(basedir, f) {
            Some(rel) => Entry {
                tag: b'R',
                key: separators(rel),
                path: f.clone(),
            },
            None => Entry {
                tag: b'A',
                key: f.clone(),
                path: f.clone(),
            },
        })
        .collect();
    out.sort_by(|a, b| (a.tag, &a.key, &a.path).cmp(&(b.tag, &b.key, &b.path)));
    out
}

/// `/` as the separator on every platform.
fn separators(key: String) -> String {
    if cfg!(windows) {
        key.replace('\\', "/")
    } else {
        key
    }
}

/// Digests one entry: a symlink's target, or a regular file's content. Any
/// other kind is an error, as is a file that changes kind while being opened.
/// Errors keep their kind and name the path.
pub(crate) fn digest(path: &str, buf: &mut [u8]) -> std::io::Result<(Kind, u128)> {
    digest_inner(Path::new(path), buf).map_err(|e| with_path(path, e))
}

fn digest_inner(path: &Path, buf: &mut [u8]) -> std::io::Result<(Kind, u128)> {
    let meta = std::fs::symlink_metadata(path)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok((Kind::Link, XxHash3_128::oneshot(&os_bytes(&target))));
    }
    if !ft.is_file() {
        return Err(unsupported());
    }
    let mut file = open_regular(path)?;
    let mut hasher = XxHash3_128::new();
    loop {
        let n = file.read(buf)?;
        if n == 0 {
            break;
        }
        hasher.write(buf.get(..n).unwrap_or(&[]));
    }
    Ok((Kind::File, hasher.finish_128()))
}

/// Opens a file `lstat` reported as regular without following a link or
/// blocking on a FIFO swapped in since, and checks it still is one.
fn open_regular(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(unsupported());
    }
    Ok(file)
}

fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        "unsupported file type (only regular files and symlinks can be fingerprinted)",
    )
}

fn with_path(path: &str, e: std::io::Error) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("{path}: {e}"))
}

/// A link target's bytes: raw on Unix, UTF-16LE elsewhere.
fn os_bytes(target: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        target.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        target
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        target.to_string_lossy().into_owned().into_bytes()
    }
}

/// Combines entry digests (in [`entries`] order) and `data` into the checksum.
pub(crate) fn combine(entries: &[(&Entry, Kind, u128)], data: &[Datum]) -> String {
    let mut h = XxHash3_128::new();
    h.write(VERSION);
    h.write(&(entries.len() as u64).to_le_bytes());
    for (e, kind, digest) in entries {
        h.write(&[e.tag]);
        h.write(&(e.key.len() as u64).to_le_bytes());
        h.write(e.key.as_bytes());
        h.write(&[kind.tag()]);
        h.write(&digest.to_be_bytes());
    }
    let mut records: Vec<Vec<u8>> = data.iter().map(Datum::encode).collect();
    records.sort();
    h.write(&(records.len() as u64).to_le_bytes());
    for r in &records {
        h.write(&(r.len() as u64).to_le_bytes());
        h.write(r);
    }
    format!("{:032x}", h.finish_128())
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u64).to_le_bytes());
    out.extend_from_slice(b);
}

/// Computes the checksum of `files` (glob results, made relative to `basedir`)
/// and `data`, digesting the files in parallel. The first failing entry in
/// canonical order is the error.
pub fn checksum_files(basedir: &str, files: &[String], data: &[Datum]) -> std::io::Result<String> {
    checksum_files_with(basedir, files, data, Workers::Auto)
}

/// [`checksum_files`] on the given threads.
pub(crate) fn checksum_files_with(
    basedir: &str,
    files: &[String],
    data: &[Datum],
    workers: Workers,
) -> std::io::Result<String> {
    Ok(snapshot_files(basedir, files, data, workers)?.hash)
}

/// A checksum and the digest of each entry it covers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The checksum.
    pub hash: String,
    /// Each entry's tag, key, kind and digest, in canonical order.
    entries: Vec<(u8, String, Kind, u128)>,
}

impl Snapshot {
    /// Sorted keys of entries added, removed or changed in content or link
    /// target between `self` and `other`. Excludes files only touched or
    /// changed and then restored to their previous content.
    pub fn changed(&self, other: &Snapshot) -> Vec<String> {
        let index = |s: &Snapshot| -> BTreeMap<(u8, String), (Kind, u128)> {
            s.entries
                .iter()
                .map(|(tag, key, kind, d)| ((*tag, key.clone()), (*kind, *d)))
                .collect()
        };
        let (a, b) = (index(self), index(other));
        let mut out: BTreeSet<String> = BTreeSet::new();
        for (k, v) in &a {
            if b.get(k) != Some(v) {
                out.insert(k.1.clone());
            }
        }
        for k in b.keys() {
            if !a.contains_key(k) {
                out.insert(k.1.clone());
            }
        }
        out.into_iter().collect()
    }
}

/// Computes the [`Snapshot`] of `files` and `data` on the given threads.
pub(crate) fn snapshot_files(
    basedir: &str,
    files: &[String],
    data: &[Datum],
    workers: Workers,
) -> std::io::Result<Snapshot> {
    let entries = entries(basedir, files);
    let digests = digest_all(&entries, workers)?;
    let combined: Vec<(&Entry, Kind, u128)> = entries
        .iter()
        .zip(digests)
        .map(|(e, (kind, d))| (e, kind, d))
        .collect();
    let hash = combine(&combined, data);
    let entries = combined
        .into_iter()
        .map(|(e, kind, d)| (e.tag, e.key.clone(), kind, d))
        .collect();
    Ok(Snapshot { hash, entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::testutil::{join, tmp, write_file};
    use std::fs;

    fn sum(dir: &str, files: &[&str], data: &[Datum]) -> String {
        let files: Vec<String> = files.iter().map(|f| join(dir, f)).collect();
        checksum_files(dir, &files, data).unwrap()
    }

    fn src(glob: &str, negate: bool) -> Datum {
        Datum::SrcRule {
            group: 0,
            negate,
            glob: glob.to_string(),
        }
    }

    #[test]
    fn regular_files_deterministic() {
        let dir = tmp();
        write_file(&dir, "a.txt", "hello");
        write_file(&dir, "b.txt", "world");
        let h1 = sum(&dir, &["a.txt", "b.txt"], &[]);
        assert_eq!(h1.len(), 32);
        assert_eq!(h1, sum(&dir, &["a.txt", "b.txt"], &[]));
    }

    #[test]
    fn different_content_different_hash() {
        let dir = tmp();
        write_file(&dir, "a.txt", "hello");
        let h1 = sum(&dir, &["a.txt"], &[]);
        write_file(&dir, "a.txt", "changed");
        assert_ne!(h1, sum(&dir, &["a.txt"], &[]));
    }

    #[test]
    fn relative_path_hashing() {
        let (dir1, dir2) = (tmp(), tmp());
        write_file(&dir1, "sub/a.txt", "hello");
        write_file(&dir2, "a.txt", "hello");
        assert_ne!(sum(&dir1, &["sub/a.txt"], &[]), sum(&dir2, &["a.txt"], &[]));
    }

    #[test]
    fn rename_changes_hash() {
        let dir = tmp();
        write_file(&dir, "a.txt", "hello");
        let h1 = sum(&dir, &["a.txt"], &[]);
        fs::rename(join(&dir, "a.txt"), join(&dir, "b.txt")).unwrap();
        assert_ne!(h1, sum(&dir, &["b.txt"], &[]));
    }

    /// The same tree in two checkouts hashes alike, whatever the order of the
    /// joined paths: here `../lib` sorts after one task directory's name and
    /// before the other's.
    #[test]
    fn relocation_keeps_the_checksum() {
        let sums: Vec<String> = ["aproj", "zproj"]
            .iter()
            .map(|name| {
                let root = tmp();
                write_file(&root, &format!("{name}/a.txt"), "a");
                write_file(&root, "lib/x.txt", "x");
                let dir = join(&root, name);
                let files = vec![join(&root, "lib/x.txt"), join(&dir, "a.txt")];
                checksum_files(&dir, &files, &[]).unwrap()
            })
            .collect();
        assert_eq!(sums.first(), sums.get(1));
    }

    #[test]
    fn input_order_does_not_matter() {
        let dir = tmp();
        write_file(&dir, "a", "1");
        write_file(&dir, "b", "2");
        assert_eq!(sum(&dir, &["a", "b"], &[]), sum(&dir, &["b", "a"], &[]));
    }

    /// Two spellings sharing a cleaned key are both hashed: `link/../x` need
    /// not be `x`.
    #[cfg(unix)]
    #[test]
    fn spellings_sharing_a_key_are_both_hashed() {
        let dir = tmp();
        write_file(&dir, "x", "here");
        write_file(&dir, "elsewhere/x", "there");
        std::os::unix::fs::symlink("elsewhere/sub", join(&dir, "link")).unwrap();
        fs::create_dir_all(join(&dir, "elsewhere/sub")).unwrap();
        let files = vec![join(&dir, "x"), format!("{dir}/link/../x")];
        let both = checksum_files(&dir, &files, &[]).unwrap();
        assert_ne!(both, sum(&dir, &["x"], &[]));
        // Changing the file only the link spelling reaches changes it.
        write_file(&dir, "elsewhere/x", "changed");
        assert_ne!(both, checksum_files(&dir, &files, &[]).unwrap());
    }

    /// Boundaries between path, content and data are framed.
    #[test]
    fn framing_separates_fields() {
        let (d1, d2) = (tmp(), tmp());
        write_file(&d1, "ab", "c");
        write_file(&d2, "a", "bc");
        assert_ne!(sum(&d1, &["ab"], &[]), sum(&d2, &["a"], &[]));

        let dir = tmp();
        let v = |s: &str| Datum::Value(s.to_string());
        assert_ne!(
            sum(&dir, &[], &[v("ab"), v("c")]),
            sum(&dir, &[], &[v("a"), v("bc")])
        );
        // A file whose content is the text of a datum.
        write_file(&dir, "f", "x");
        assert_ne!(sum(&dir, &["f"], &[]), sum(&dir, &[], &[v("x")]));
        // An empty file is an entry; no file is none.
        write_file(&dir, "e", "");
        assert_ne!(sum(&dir, &["e"], &[]), sum(&dir, &[], &[]));
        // An exclusion of `x` differs from a positive glob `!x`.
        assert_ne!(
            sum(&dir, &[], &[src("x", true)]),
            sum(&dir, &[], &[src("!x", false)])
        );
        assert_eq!(src("x", true).display(), src("!x", false).display());
    }

    /// Known-answer vectors pinning format v1.
    #[test]
    fn v1_vectors() {
        let dir = tmp();
        assert_eq!(sum(&dir, &[], &[]), "f2f36b292df325ee8abf7808417ca552");
        write_file(&dir, "a", "x");
        assert_eq!(sum(&dir, &["a"], &[]), "97c0cedcdbcae1fead812f0afe44a08b");
        let data = [
            src("*.c", false),
            Datum::Cmd {
                index: 0,
                cmd: "cc".to_string(),
            },
        ];
        assert_eq!(sum(&dir, &["a"], &data), "370117a7394da7305e63381cc1f1327b");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("a", join(&dir, "l")).unwrap();
            assert_eq!(
                sum(&dir, &["a", "l"], &[]),
                "31261051a70aadafa527752033c970de"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_hashes_target_not_content() {
        let (d1, d2) = (tmp(), tmp());
        fs::write(join(&d1, "target.txt"), b"data").unwrap();
        std::os::unix::fs::symlink(join(&d1, "target.txt"), join(&d1, "link.txt")).unwrap();
        fs::write(join(&d2, "link.txt"), b"data").unwrap();
        assert_ne!(sum(&d1, &["link.txt"], &[]), sum(&d2, &["link.txt"], &[]));
    }

    /// A link and a file holding its target's text differ.
    #[cfg(unix)]
    #[test]
    fn symlink_and_file_with_its_target_differ() {
        let (d1, d2) = (tmp(), tmp());
        std::os::unix::fs::symlink("x", join(&d1, "a")).unwrap();
        fs::write(join(&d2, "a"), b"x").unwrap();
        assert_ne!(sum(&d1, &["a"], &[]), sum(&d2, &["a"], &[]));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_retarget_changes_hash() {
        let dir = tmp();
        let link = join(&dir, "link.txt");
        std::os::unix::fs::symlink("t1", &link).unwrap();
        let h1 = sum(&dir, &["link.txt"], &[]);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("t2", &link).unwrap();
        assert_ne!(h1, sum(&dir, &["link.txt"], &[]));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_link_targets_differ() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tmp();
        let link = join(&dir, "l");
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(b"\xff"), &link).unwrap();
        let h1 = sum(&dir, &["l"], &[]);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(b"\xfe"), &link).unwrap();
        assert_ne!(h1, sum(&dir, &["l"], &[]));
    }

    #[test]
    fn snapshots_name_changed_entries() {
        let dir = tmp();
        write_file(&dir, "a", "1");
        write_file(&dir, "b", "2");
        write_file(&dir, "c", "3");
        let snap = |names: &[&str]| {
            let files: Vec<String> = names.iter().map(|f| join(&dir, f)).collect();
            snapshot_files(&dir, &files, &[], Workers::Auto).unwrap()
        };
        let before = snap(&["a", "b", "c"]);
        // Touched with the same content: not named.
        write_file(&dir, "a", "1");
        write_file(&dir, "b", "changed");
        write_file(&dir, "d", "4");
        let after = snap(&["a", "b", "d"]);
        assert_eq!(before.changed(&after), ["b", "c", "d"]);
        assert!(before.changed(&before).is_empty());
    }

    #[test]
    fn data_strings() {
        let dir = tmp();
        let v = |s: &str| Datum::Value(s.to_string());
        assert_ne!(
            sum(&dir, &[], &[v("foo"), v("bar")]),
            sum(&dir, &[], &[v("foo"), v("baz")])
        );
        // Order-independent, as the rules are a set.
        assert_eq!(
            sum(&dir, &[], &[v("a"), v("b")]),
            sum(&dir, &[], &[v("b"), v("a")])
        );
    }

    #[test]
    fn missing_file_errors_with_its_path() {
        let dir = tmp();
        let missing = join(&dir, "nope.txt");
        let err = checksum_files(&dir, std::slice::from_ref(&missing), &[]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().starts_with(&format!("{missing}: ")),
            "{err}"
        );
    }

    /// With several failures, the first in path order is reported.
    #[test]
    fn first_failure_in_path_order() {
        let dir = tmp();
        write_file(&dir, "b", "1");
        let files = vec![join(&dir, "z"), join(&dir, "b"), join(&dir, "c")];
        let err = checksum_files(&dir, &files, &[]).unwrap_err();
        assert!(err.to_string().starts_with(&join(&dir, "c")), "{err}");
    }

    #[test]
    fn directory_is_unsupported() {
        let dir = tmp();
        fs::create_dir_all(join(&dir, "d")).unwrap();
        let err = checksum_files(&dir, &[join(&dir, "d")], &[]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_unsupported() {
        let dir = tmp();
        let fifo = join(&dir, "p");
        let c = std::ffi::CString::new(fifo.clone()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = checksum_files(&dir, &[fifo], &[]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("unsupported file type"), "{err}");
    }
}

//! Enforcing a [`Policy`] with Landlock.
//!
//! The calling thread restricts itself: Landlock domains are per thread, so the
//! rest of the engine keeps its access, while every thread and process the
//! command thread starts inherits the restriction. Each policy is one stacked
//! domain, so a command runs under the intersection of the policies above it.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use super::policy::{Policy, access};

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

/// The lowest ABI used: 2 adds `REFER`, without which a file can never be
/// renamed or linked into another directory.
pub(crate) const MIN_ABI: i64 = 2;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI version the kernel offers, or an error when it has none.
pub(crate) fn abi() -> io::Result<i64> {
    // SAFETY: a null attribute with size 0 and the version flag only queries.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

/// The rights the kernel's ABI knows about: ABI 2 every right up to `REFER`,
/// 3 adds `TRUNCATE`, 5 adds `IOCTL_DEV`.
fn handled(abi: i64) -> u64 {
    match abi {
        ..=2 => access::ALL & !(access::TRUNCATE | access::IOCTL_DEV),
        3 | 4 => access::ALL & !access::IOCTL_DEV,
        _ => access::ALL,
    }
}

/// Builds the ruleset for `policy`.
fn ruleset(policy: &Policy, abi: i64) -> io::Result<OwnedFd> {
    let handled = handled(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
    };
    // SAFETY: `attr` is a valid ruleset attribute of the size passed.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0u32,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(|_| io::Error::other("ruleset fd out of range"))?;
    // SAFETY: the kernel just returned this descriptor and nothing else owns it.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };

    for (path, rights) in &policy.rules {
        // Outside the root a rule is best effort: one that cannot be granted
        // (a dead or foreign FUSE mount beside an ancestor) only denies what
        // no build should need. Inside, a skipped rule would surface as an
        // unexplained denial: fail naming the path instead.
        let best_effort = !path.starts_with(&policy.root);
        let cpath = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has a NUL byte", path.display()),
            )
        })?;
        // Not following a final symlink: one put there since the walk could
        // lead back into the root.
        // SAFETY: `cpath` is a valid NUL-terminated path.
        let raw = unsafe {
            libc::open(
                cpath.as_ptr(),
                libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let err = io::Error::last_os_error();
            // Gone since the walk: nothing to grant.
            if best_effort || matches!(err.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) {
                continue;
            }
            return Err(io::Error::new(
                err.kind(),
                format!("opening {}: {err}", path.display()),
            ));
        }
        // SAFETY: `open` just returned this descriptor and nothing else owns it.
        let parent = unsafe { OwnedFd::from_raw_fd(raw) };
        // The type of what was opened, not of what the path names by now.
        // SAFETY: an all-zero `stat` is valid, and the kernel fills it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `st` is a valid `stat` for the kernel to fill.
        if unsafe { libc::fstat(parent.as_raw_fd(), &mut st) } < 0 {
            if best_effort {
                continue;
            }
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("checking {}: {err}", path.display()),
            ));
        }
        let kind = st.st_mode & libc::S_IFMT;
        if kind == libc::S_IFLNK {
            // A symlink put there since the walk: nothing to grant.
            continue;
        }
        let is_dir = kind == libc::S_IFDIR;
        let mut allowed = rights & handled;
        if !is_dir {
            allowed &= access::FILE;
        }
        if allowed == 0 {
            continue;
        }
        let rule = PathBeneathAttr {
            allowed_access: allowed,
            parent_fd: parent.as_raw_fd(),
        };
        // SAFETY: `rule` is a valid path-beneath attribute; both fds are open.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                ruleset.as_raw_fd(),
                RULE_PATH_BENEATH,
                &rule as *const PathBeneathAttr,
                0u32,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("adding a rule for {}: {err}", path.display()),
            ));
        }
    }
    Ok(ruleset)
}

/// Restricts the calling thread to every policy in `layers`, outermost first.
pub(crate) fn restrict(layers: &[std::sync::Arc<Policy>], abi: i64) -> io::Result<()> {
    // Every ruleset is built before the first restriction, which would
    // otherwise apply to opening the next one's paths.
    let mut fds = Vec::with_capacity(layers.len());
    for policy in layers {
        let ruleset = policy
            .ruleset
            .get_or_init(|| ruleset(policy, abi).map_err(|e| e.to_string()));
        match ruleset {
            Ok(fd) => fds.push(fd.as_raw_fd()),
            Err(e) => {
                return Err(io::Error::other(format!(
                    "building the Landlock rules of {:?}: {e}",
                    policy.task
                )));
            }
        }
    }
    super::set_no_new_privs()
        .map_err(|e| io::Error::new(e.kind(), format!("setting no_new_privs for Landlock: {e}")))?;
    for (policy, fd) in layers.iter().zip(fds) {
        // SAFETY: `fd` is an open ruleset descriptor, owned by its policy.
        let ret = unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) };
        if ret < 0 {
            let err = io::Error::last_os_error();
            // The kernel caps how many Landlock domains a thread stacks, and
            // each sandboxed level adds one.
            let hint = if err.raw_os_error() == Some(libc::E2BIG) {
                "; too many nested sandboxed tasks: the kernel limits how many \
                 Landlock domains stack"
            } else {
                ""
            };
            return Err(io::Error::new(
                err.kind(),
                format!(
                    "applying the Landlock rules of {:?}: {err}{hint}",
                    policy.task
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unopenable_path_fails_only_inside_the_root() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(abi) = abi() else {
            eprintln!("skipping: no Landlock here");
            return;
        };
        // SAFETY: plain getter, no arguments.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("skipping: root opens a mode 000 directory");
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "task-landlock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("locked")).unwrap();
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        for locked in [dir.join("locked"), root.join("locked")] {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        }
        let outside = Policy::for_rules(
            root.clone(),
            vec![
                (dir.join("locked/x"), access::ALL),
                (root.clone(), access::READ_DIR),
            ],
        );
        let inside = Policy::for_rules(root.clone(), vec![(root.join("locked/x"), access::ALL)]);
        let (outside, inside) = (ruleset(&outside, abi), ruleset(&inside, abi));
        for locked in [dir.join("locked"), root.join("locked")] {
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(outside.is_ok(), "{outside:?}");
        let err = inside.unwrap_err().to_string();
        assert!(err.starts_with("opening "), "{err}");
    }
}

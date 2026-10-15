//! Enforcing (or auditing) [`Policy`] layers with a seccomp user-notification
//! supervisor, where Landlock is unavailable.
//!
//! The command thread installs a filter that hands every path-taking syscall
//! (opens, exec, creating, linking, removing and renaming entries) to a
//! supervisor thread, which resolves the paths as the caller would, checks them
//! against every layer and answers: let the syscall continue, or fail it with
//! `EACCES`. Every thread and process the command thread starts inherits the
//! filter. The supervisor is started from an unfiltered thread of the engine,
//! before the filter exists, so its own syscalls are not filtered.
//!
//! Paths are resolved one component at a time from the caller's root, working
//! directory or directory descriptor, opened through `/proc/<pid>`: `..` is
//! taken after the symlinks before it, `/proc/self` is the caller, and the
//! links under `/proc/<pid>` (`fd/N`, `cwd`, ...) lead where they lead for the
//! caller. A syscall whose paths cannot be resolved — the caller's memory
//! cannot be read, say — is denied, or reported as unchecked when auditing.
//! Every syscall of another architecture or of x32, whose numbers name other
//! syscalls, is handed over too: failed with `EACCES` when enforcing, let
//! through unchecked when auditing, and either way noted once per
//! architecture, without counting as a denial.
//!
//! Reading the caller's memory and its `/proc/<pid>` links takes the same
//! permission as `ptrace`: a process that made itself non-dumpable
//! (`PR_SET_DUMPABLE` 0, as `ssh-agent` does), or one that is no longer
//! a descendant of this process under `kernel.yama.ptrace_scope` 1 (a daemon
//! that double-forked and was reparented), has every checked syscall denied.
//! This process is not made a child subreaper to keep such daemons
//! descendants: it would then adopt every orphan, which makes the reap sweep
//! (`crate::reap`) decline to run and leaves exited daemons as zombies until
//! the engine exits.
//!
//! This is a check on a build's declared inputs, not a security boundary: the
//! syscall runs after the check, on a path that could have changed since, and
//! syscalls the filter does not list (or `io_uring`) are not seen. Each checked
//! syscall costs two context switches and a few syscalls per path component,
//! hence the fallback status. A process that outlives its command — a daemon —
//! keeps being checked while the engine runs; its syscalls fail with `ENOSYS`
//! once the engine is gone.
//!
//! One process tree can carry only one listener: a sandboxed `task` started by
//! a command already under this supervisor runs under the enclosing layers.

use std::collections::HashSet;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::policy::{Policy, access};
use super::{Denial, Denials};

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JGE_K: u16 = 0x35;
const BPF_RET_K: u16 = 0x06;
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
/// `__X32_SYSCALL_BIT`: an x32 syscall's number has it set, under the native
/// `AUDIT_ARCH_X86_64`. No native syscall number reaches it on any arch.
const X32_SYSCALL_BIT: u32 = 0x4000_0000;

/// `openat2`'s `resolve` flags (`include/uapi/linux/openat2.h`).
const RESOLVE_NO_XDEV: u64 = 0x01;
const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
const RESOLVE_IN_ROOT: u64 = 0x10;
const RESOLVE_CACHED: u64 = 0x20;
/// The symlinks a lookup follows before failing with `ELOOP`.
const MAX_LINKS: u32 = 40;
/// The inode number of procfs's root.
const PROC_ROOT_INO: u64 = 1;

/// A path argument: the index of its directory descriptor (`None`: the working
/// directory), and of the path.
type At = (Option<usize>, usize);

/// How a syscall's arguments name the paths it acts on.
#[derive(Clone, Copy)]
enum Op {
    /// `open(path, flags)`, `openat(dirfd, path, flags)`, `creat(path)`.
    Open { at: At, flags: Flags },
    /// `execve(path)`, `execveat(dirfd, path, .., .., flags)`.
    Exec { at: At, flags: Option<usize> },
    /// A new entry: `mkdir`, `mknod`, `symlink`'s link.
    Make { at: At, kind: Kind },
    /// `link(old, new)`, `linkat(.., flags)`.
    Link {
        old: At,
        new: At,
        flags: Option<usize>,
    },
    /// A removed entry: `unlink`, `rmdir`, `unlinkat(.., flags)`.
    Remove { at: At, flags: Option<usize> },
    /// `rename(old, new)`, `renameat`, `renameat2(.., flags)`.
    Rename {
        old: At,
        new: At,
        flags: Option<usize>,
    },
    /// `truncate(path)`.
    Truncate { at: At },
}

#[derive(Clone, Copy)]
enum Flags {
    Arg(usize),
    /// `openat2`: the `open_how` argument.
    How(usize),
    Creat,
}

/// What kind of entry a `Make` creates.
#[derive(Clone, Copy)]
enum Kind {
    Right(u64),
    /// `mknod`: from the file type in this mode argument.
    Mode(usize),
}

/// The syscalls the filter hands over, their names, and how to read them.
fn ops() -> Vec<(libc::c_long, &'static str, Op)> {
    let mut ops = vec![
        (
            libc::SYS_openat,
            "openat",
            Op::Open {
                at: (Some(0), 1),
                flags: Flags::Arg(2),
            },
        ),
        (
            libc::SYS_openat2,
            "openat2",
            Op::Open {
                at: (Some(0), 1),
                flags: Flags::How(2),
            },
        ),
        (
            libc::SYS_execve,
            "execve",
            Op::Exec {
                at: (None, 0),
                flags: None,
            },
        ),
        (
            libc::SYS_execveat,
            "execveat",
            Op::Exec {
                at: (Some(0), 1),
                flags: Some(4),
            },
        ),
        (
            libc::SYS_mkdirat,
            "mkdirat",
            Op::Make {
                at: (Some(0), 1),
                kind: Kind::Right(access::MAKE_DIR),
            },
        ),
        (
            libc::SYS_mknodat,
            "mknodat",
            Op::Make {
                at: (Some(0), 1),
                kind: Kind::Mode(2),
            },
        ),
        (
            libc::SYS_symlinkat,
            "symlinkat",
            Op::Make {
                at: (Some(1), 2),
                kind: Kind::Right(access::MAKE_SYM),
            },
        ),
        (
            libc::SYS_linkat,
            "linkat",
            Op::Link {
                old: (Some(0), 1),
                new: (Some(2), 3),
                flags: Some(4),
            },
        ),
        (
            libc::SYS_unlinkat,
            "unlinkat",
            Op::Remove {
                at: (Some(0), 1),
                flags: Some(2),
            },
        ),
        (
            libc::SYS_renameat2,
            "renameat2",
            Op::Rename {
                old: (Some(0), 1),
                new: (Some(2), 3),
                flags: Some(4),
            },
        ),
        (
            libc::SYS_truncate,
            "truncate",
            Op::Truncate { at: (None, 0) },
        ),
    ];
    #[cfg(target_arch = "x86_64")]
    ops.extend([
        (
            libc::SYS_open,
            "open",
            Op::Open {
                at: (None, 0),
                flags: Flags::Arg(1),
            },
        ),
        (
            libc::SYS_creat,
            "creat",
            Op::Open {
                at: (None, 0),
                flags: Flags::Creat,
            },
        ),
        (
            libc::SYS_mkdir,
            "mkdir",
            Op::Make {
                at: (None, 0),
                kind: Kind::Right(access::MAKE_DIR),
            },
        ),
        (
            libc::SYS_mknod,
            "mknod",
            Op::Make {
                at: (None, 0),
                kind: Kind::Mode(1),
            },
        ),
        (
            libc::SYS_symlink,
            "symlink",
            Op::Make {
                at: (None, 1),
                kind: Kind::Right(access::MAKE_SYM),
            },
        ),
        (
            libc::SYS_link,
            "link",
            Op::Link {
                old: (None, 0),
                new: (None, 1),
                flags: None,
            },
        ),
        (
            libc::SYS_unlink,
            "unlink",
            Op::Remove {
                at: (None, 0),
                flags: None,
            },
        ),
        (
            libc::SYS_rmdir,
            "rmdir",
            Op::Remove {
                at: (None, 0),
                flags: None,
            },
        ),
        (
            libc::SYS_rename,
            "rename",
            Op::Rename {
                old: (None, 0),
                new: (None, 1),
                flags: None,
            },
        ),
        (
            libc::SYS_renameat,
            "renameat",
            Op::Rename {
                old: (Some(0), 1),
                new: (Some(2), 3),
                flags: None,
            },
        ),
    ]);
    ops
}

fn insn(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The filter: hand the listed syscalls of the native architecture over, and
/// every syscall of another one (i386 on x86_64, say) or of x32, whose numbers
/// differ; let everything else through.
fn program(ops: &[(libc::c_long, &'static str, Op)]) -> io::Result<Vec<libc::sock_filter>> {
    let n = ops.len();
    let Some(past_ops) = n.checked_add(1).and_then(|s| u8::try_from(s).ok()) else {
        return Err(io::Error::other("seccomp filter too large"));
    };
    let mut prog = vec![
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_ARCH),
        insn(BPF_JEQ_K, 1, 0, AUDIT_ARCH),
        insn(BPF_RET_K, 0, 0, libc::SECCOMP_RET_USER_NOTIF),
        insn(BPF_LD_W_ABS, 0, 0, OFFSET_NR),
        // Over the comparisons and the allow to the notify.
        insn(BPF_JGE_K, past_ops, 0, X32_SYSCALL_BIT),
    ];
    for (i, (nr, _, _)) in ops.iter().enumerate() {
        // Jump over the remaining comparisons and the allow to the notify.
        let skip = n.checked_sub(i).and_then(|s| u8::try_from(s).ok());
        let (Some(skip), Ok(nr)) = (skip, u32::try_from(*nr)) else {
            return Err(io::Error::other("seccomp filter too large"));
        };
        prog.push(insn(BPF_JEQ_K, skip, 0, nr));
    }
    prog.push(insn(BPF_RET_K, 0, 0, libc::SECCOMP_RET_ALLOW));
    prog.push(insn(BPF_RET_K, 0, 0, libc::SECCOMP_RET_USER_NOTIF));
    Ok(prog)
}

/// Checks, once per process, that the supervisor works here: the filter
/// installs, on a thread of its own that ends right after; a syscall it hands
/// over can be let through (`SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which kernels
/// before 5.5 lack although they have listeners); and a child's memory can be
/// read, by either means [`Process`] has, which `kernel.yama.ptrace_scope` 2
/// or more forbids. A process already under a supervisor passes the first
/// two: that one checks its commands.
pub(crate) fn probe() -> Result<(), String> {
    static PROBED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PROBED
        .get_or_init(|| {
            std::thread::Builder::new()
                .name("task-sandbox-probe".to_string())
                .spawn(probe_here)
                .map_err(|e| e.to_string())?
                .join()
                .unwrap_or_else(|_| Err("the probe thread panicked".to_string()))?;
            child_memory_readable()
        })
        .clone()
}

/// What [`child_memory_readable`] reads in the child.
static MARK: u64 = 0x7461_736b_2d70_726f;

/// Checks that this process can read a child's memory, as the supervisor
/// reads the paths a command's syscalls name.
fn child_memory_readable() -> Result<(), String> {
    // SAFETY: the child only waits to be killed, with async-signal-safe calls,
    // as a fork of a multithreaded process must.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        loop {
            // SAFETY: plain pause, async-signal-safe.
            unsafe { libc::pause() };
        }
    }
    let mut buf = [0u8; 8];
    let read = u32::try_from(pid)
        .map_err(|_| io::Error::other("pid out of range"))
        .and_then(|p| read_exact(&Process::new(p), std::ptr::addr_of!(MARK) as u64, &mut buf));
    // SAFETY: `pid` is this process's child, killed and reaped here.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    match read {
        Ok(()) if u64::from_ne_bytes(buf) == MARK => Ok(()),
        Ok(()) => Err("reading a child's memory returned other bytes".to_string()),
        Err(e) => Err(format!(
            "cannot read the commands' memory ({e}); is kernel.yama.ptrace_scope 2 or more?"
        )),
    }
}

/// [`probe`] on the calling thread, which is filtered afterwards: once the
/// helper closes the listener, its filtered syscalls fail with `ENOSYS`.
fn probe_here() -> Result<(), String> {
    // The helper answers the notification. Started before the filter is
    // installed, it is not filtered itself.
    let (tx, rx) = std::sync::mpsc::channel::<OwnedFd>();
    let helper = std::thread::Builder::new()
        .name("task-sandbox-probe".to_string())
        .spawn(move || match rx.recv() {
            Ok(listener) => answer_once(&listener),
            // The filter did not install: the prober reports why.
            Err(_) => Ok(()),
        })
        .map_err(|e| e.to_string())?;
    let installed = install();
    let listener = match installed {
        Ok(listener) => listener,
        Err(e) => {
            drop(tx);
            let _ = helper.join(); // Nothing to report: it got no listener.
            return if e.raw_os_error() == Some(libc::EBUSY) {
                Ok(())
            } else {
                Err(e.to_string())
            };
        }
    };
    if tx.send(listener).is_err() {
        return Err("the probe helper died".to_string());
    }
    // A syscall the filter hands over: the helper lets it through, or, failing
    // to, fails it or closes the listener, so that it returns either way.
    // SAFETY: the path is NUL-terminated and static.
    let fd = owned(unsafe {
        libc::openat(
            libc::AT_FDCWD,
            c"/".as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC,
        )
    });
    helper
        .join()
        .unwrap_or_else(|_| Err("the probe helper panicked".to_string()))?;
    fd.map(drop)
        .map_err(|e| format!("a syscall let through failed ({e})"))
}

/// Answers the first notification on `listener` by letting the syscall
/// through, within a few seconds.
fn answer_once(listener: &OwnedFd) -> Result<(), String> {
    let mut pfd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let interrupted = || io::Error::last_os_error().kind() == io::ErrorKind::Interrupted;
    // A signal may interrupt the wait: the deadline restarts, which is fine
    // for a bound on something that takes microseconds.
    loop {
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, 5000) };
        if ready > 0 {
            break;
        }
        if ready == 0 || !interrupted() {
            return Err("no syscall was handed over".to_string());
        }
    }
    // SAFETY: an all-zero `seccomp_notif` is valid, and the kernel wants it
    // zeroed.
    let mut req: libc::seccomp_notif = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `req` is a valid `seccomp_notif` for the kernel to fill.
        let ret = unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV,
                &mut req,
            )
        };
        if ret >= 0 {
            break;
        }
        if !interrupted() {
            return Err(format!(
                "receiving a notification: {}",
                io::Error::last_os_error()
            ));
        }
        // SAFETY: as above; the kernel wants it zeroed again.
        req = unsafe { std::mem::zeroed() };
    }
    let mut resp = libc::seccomp_notif_resp {
        id: req.id,
        val: 0,
        error: 0,
        flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
    };
    send(listener, &mut resp).map_err(|e| {
        resp.flags = 0;
        resp.error = -libc::ENOSYS;
        // Closing the listener on return answers it anyway.
        let _ = send(listener, &mut resp);
        format!(
            "the kernel cannot let a checked syscall through ({e}); Linux 5.5 or later is needed"
        )
    })
}

/// Installs the filter on the calling thread and returns its listener.
pub(crate) fn install() -> io::Result<OwnedFd> {
    super::set_no_new_privs()?;
    let ops = ops();
    let mut prog = program(&ops)?;
    let fprog = libc::sock_fprog {
        len: u16::try_from(prog.len()).map_err(|_| io::Error::other("seccomp filter too large"))?,
        filter: prog.as_mut_ptr(),
    };
    let set = |flags: libc::c_ulong| {
        // SAFETY: `fprog` points at `prog`, which outlives the call.
        unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                flags,
                &fprog as *const libc::sock_fprog,
            )
        }
    };
    // Once the supervisor has received a notification, only a fatal signal
    // interrupts the caller's wait (Linux 5.19): otherwise a signal makes the
    // kernel restart the syscall, and the supervisor sees it again.
    let mut fd =
        set(libc::SECCOMP_FILTER_FLAG_NEW_LISTENER | libc::SECCOMP_FILTER_FLAG_WAIT_KILLABLE_RECV);
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        fd = set(libc::SECCOMP_FILTER_FLAG_NEW_LISTENER);
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = i32::try_from(fd).map_err(|_| io::Error::other("listener fd out of range"))?;
    // SAFETY: the kernel just returned this descriptor and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Answers the filter's notifications until no process uses it any more.
pub(crate) fn supervise(
    listener: OwnedFd,
    layers: Vec<Arc<Policy>>,
    enforce: bool,
    denials: Denials,
) {
    let ops = ops();
    let mut seen: HashSet<Denial> = HashSet::new();
    loop {
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid pollfd for the duration of the call.
        let ready = unsafe { libc::poll(&mut pfd, 1, -1) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return stop(&mut seen, &denials, "waiting for a notification", &e);
        }
        if pfd.revents & libc::POLLIN == 0 {
            // POLLHUP: every process using the filter is gone.
            return;
        }
        // SAFETY: an all-zero `seccomp_notif` is valid, and the kernel wants it
        // zeroed.
        let mut req: libc::seccomp_notif = unsafe { std::mem::zeroed() };
        // SAFETY: `req` is a valid `seccomp_notif` for the kernel to fill.
        let ret = unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV,
                &mut req,
            )
        };
        if ret < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                // The caller died before we read it, or a signal interrupted
                // the read.
                Some(libc::ENOENT | libc::EINTR) => continue,
                _ => return stop(&mut seen, &denials, "receiving a notification", &e),
            }
        }

        let nr = libc::c_long::from(req.data.nr);
        let native = req.data.arch == AUDIT_ARCH;
        let (syscall, found, foreign) = match ops.iter().find(|(n, _, _)| *n == nr) {
            Some((_, name, op)) if native => {
                let caller = Caller {
                    pid: req.pid,
                    mem: &Process::notified(req.pid, &listener, req.id),
                };
                let found = judge(&layers, name, accesses(&caller, &req.data.args, *op));
                (*name, found, None)
            }
            // Another architecture's or x32's numbering: what the syscall does
            // with which paths is not known here. Noted once per architecture,
            // not per syscall: one such program makes many.
            _ if !native || nr >= libc::c_long::from(X32_SYSCALL_BIT) => {
                let syscalls = if native {
                    "x32 syscalls".to_string()
                } else {
                    format!("syscalls of architecture {:#x}", req.data.arch)
                };
                (
                    "a foreign syscall",
                    Vec::new(),
                    Some(Denial::Foreign { syscalls }),
                )
            }
            // A native syscall the filter does not hand over: none arrives,
            // but one would be denied rather than let through unread.
            _ => {
                let syscall = "a syscall the filter does not list";
                let why = format!("number {nr}");
                (syscall, vec![Denial::Unchecked { syscall, why }], None)
            }
        };
        // The caller may have died while its paths were resolved, and its pid
        // been reused: then what was read belongs to another process, and the
        // kernel has dropped the notification anyway.
        if !id_valid(&listener, req.id) {
            continue;
        }
        let denied = !found.is_empty() || foreign.is_some();
        for d in found {
            record(&mut seen, &denials, d);
        }
        if let Some(d) = foreign {
            note(&mut seen, &denials, d);
        }

        let mut resp = libc::seccomp_notif_resp {
            id: req.id,
            val: 0,
            error: 0,
            flags: 0,
        };
        if denied && enforce {
            resp.error = -libc::EACCES;
        } else {
            resp.flags = libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32;
        }
        match send(&listener, &mut resp) {
            // The caller died meanwhile.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
            // A kernel before 5.5 knows no `FLAG_CONTINUE`: fail the syscall
            // rather than leave the caller waiting for an answer forever.
            Err(e) if resp.flags != 0 => {
                resp.flags = 0;
                resp.error = -libc::EACCES;
                let why = format!("the kernel cannot let it through ({e})");
                record(&mut seen, &denials, Denial::Unchecked { syscall, why });
                // Nothing more to do if this fails too: the caller died, or the
                // listener is unusable and the caller gets `ENOSYS` once it is
                // gone.
                let _ = send(&listener, &mut resp);
            }
            Err(_) | Ok(()) => {}
        }
    }
}

/// Ends supervision, which `listener` going out of scope with the return
/// completes: the kernel then fails the callers' pending and later checked
/// syscalls with `ENOSYS`, so none is left waiting. Records why, so that the
/// command fails rather than passing on syscalls nobody checked.
fn stop(seen: &mut HashSet<Denial>, denials: &Denials, doing: &str, e: &io::Error) {
    let why = format!("the supervisor stopped {doing}: {e}");
    record(
        seen,
        denials,
        Denial::Unchecked {
            syscall: "every later checked syscall",
            why,
        },
    );
}

/// Answers a notification.
fn send(listener: &OwnedFd, resp: &mut libc::seccomp_notif_resp) -> io::Result<()> {
    // SAFETY: `resp` is a valid response for the kernel to read.
    let ret = unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            libc::SECCOMP_IOCTL_NOTIF_SEND,
            resp as *mut libc::seccomp_notif_resp,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Counts every denial, including repeats, against the command making it.
/// Records each distinct `d` for the engine to report once per supervisor.
fn record(seen: &mut HashSet<Denial>, denials: &Denials, d: Denial) {
    let mut out = denials
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.count = out.count.saturating_add(1);
    if seen.insert(d.clone()) {
        out.denials.push(d);
    }
}

/// Records `d` for the engine to report, once per supervisor, without counting
/// it as a denial.
fn note(seen: &mut HashSet<Denial>, denials: &Denials, d: Denial) {
    if seen.insert(d.clone()) {
        denials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .denials
            .push(d);
    }
}

/// Reports whether the notification `id` still waits for an answer.
fn id_valid(listener: &OwnedFd, mut id: u64) -> bool {
    // SAFETY: `id` is a valid u64 for the kernel to read.
    let ret = unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            libc::SECCOMP_IOCTL_NOTIF_ID_VALID,
            &mut id,
        )
    };
    ret == 0
}

/// One thing a notified syscall needs the layers to allow.
#[derive(Debug, PartialEq, Eq)]
enum Check {
    /// `rights` on `path`; `what` describes the access.
    Access {
        path: PathBuf,
        rights: u64,
        what: &'static str,
    },
    /// `entry` linked or moved from directory `from` into directory `to`.
    Refer {
        entry: PathBuf,
        from: PathBuf,
        to: PathBuf,
        dir: bool,
        what: &'static str,
    },
}

/// Why a syscall's paths could not be resolved.
#[derive(Debug)]
struct Unchecked(String);

impl From<io::Error> for Unchecked {
    fn from(e: io::Error) -> Self {
        if e.raw_os_error() == Some(libc::EPERM) {
            // The ptrace access check on the caller's memory or `/proc` links.
            return Self(format!(
                "{e}: the process is not dumpable (PR_SET_DUMPABLE 0, as ssh-agent \
                 is), or kernel.yama.ptrace_scope 1 and it is no longer a descendant \
                 of task (a daemon reparented away); Landlock has no such limit"
            ));
        }
        Self(e.to_string())
    }
}

/// The denials `layers` make of a notified syscall: one per refused check,
/// naming the first layer that refuses it, or one for a syscall that could not
/// be checked.
fn judge(
    layers: &[Arc<Policy>],
    syscall: &'static str,
    checks: Result<Vec<Check>, Unchecked>,
) -> Vec<Denial> {
    let checks = match checks {
        Ok(checks) => checks,
        Err(Unchecked(why)) => return vec![Denial::Unchecked { syscall, why }],
    };
    let mut out = Vec::new();
    for check in &checks {
        let refused = |l: &&Arc<Policy>| match check {
            Check::Access { path, rights, .. } => !l.allows(path, *rights),
            Check::Refer { from, to, dir, .. } => !l.refers(from, to, *dir),
        };
        if let Some(layer) = layers.iter().find(refused) {
            let (access, path) = match check {
                Check::Access { path, what, .. } => (*what, path.clone()),
                Check::Refer { entry, what, .. } => (*what, entry.clone()),
            };
            out.push(Denial::Refused {
                task: layer.task.clone(),
                access,
                path,
            });
        }
    }
    out
}

/// Reads another process's memory; a trait so that tests can make it fail.
trait Memory {
    fn read(&self, addr: u64, buf: &mut [u8]) -> io::Result<usize>;
}

/// The memory of the process (or thread) `pid`, read with `process_vm_readv`,
/// or through `/proc/<pid>/mem` on a kernel without it
/// (`CONFIG_CROSS_MEMORY_ATTACH` unset, as in some microVM guests).
struct Process<'a> {
    pid: u32,
    /// The read's notification. Checking it after opening `/proc/<pid>/mem`
    /// ensures the descriptor belongs to the caller, not a reused pid. The
    /// supervisor checks again before answering, covering `process_vm_readv`
    /// too.
    notification: Option<(&'a OwnedFd, u64)>,
    /// `/proc/<pid>/mem`, opened on first use, or the `errno` that failed it.
    mem: std::cell::OnceCell<Result<OwnedFd, i32>>,
}

/// Whether `process_vm_readv` failed with `ENOSYS`, so that later reads go
/// through `/proc/<pid>/mem` straight away.
static NO_VM_READV: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether reads skip `process_vm_readv`, as without it: forced in test
/// builds by `TASK_TEST_SECCOMP_PROC_MEM`.
fn no_vm_readv() -> bool {
    #[cfg(any(test, debug_assertions))]
    {
        static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *FORCED.get_or_init(|| std::env::var_os("TASK_TEST_SECCOMP_PROC_MEM").is_some()) {
            return true;
        }
    }
    NO_VM_READV.load(std::sync::atomic::Ordering::Relaxed)
}

impl<'a> Process<'a> {
    /// Process `pid`, outside any notification.
    fn new(pid: u32) -> Self {
        Self {
            pid,
            notification: None,
            mem: std::cell::OnceCell::new(),
        }
    }

    /// Process `pid`, whose syscall `listener` notified as `id`.
    fn notified(pid: u32, listener: &'a OwnedFd, id: u64) -> Self {
        Self {
            notification: Some((listener, id)),
            ..Self::new(pid)
        }
    }

    fn read_vm(&self, addr: u64, buf: &mut [u8]) -> io::Result<usize> {
        let local = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let pid =
            libc::pid_t::try_from(self.pid).map_err(|_| io::Error::other("pid out of range"))?;
        // SAFETY: `local` covers `buf`; the remote range is only read by the
        // kernel.
        let n = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
        match usize::try_from(n) {
            Ok(0) => Err(io::Error::from_raw_os_error(libc::EFAULT)),
            Ok(n) => Ok(n),
            Err(_) => Err(io::Error::last_os_error()),
        }
    }

    /// Opens `/proc/<pid>/mem`, failing as `process_vm_readv` would: `EPERM`
    /// for the ptrace access check, `ESRCH` once the caller or its
    /// notification is gone.
    fn open_mem(&self) -> Result<OwnedFd, i32> {
        let path = CString::new(format!("/proc/{}/mem", self.pid)).map_err(|_| libc::EINVAL)?;
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let fd = owned(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) })
            .map_err(|e| open_errno(e.raw_os_error()))?;
        match self.notification {
            Some((listener, id)) if !id_valid(listener, id) => Err(libc::ESRCH),
            _ => Ok(fd),
        }
    }

    fn read_proc(&self, addr: u64, buf: &mut [u8]) -> io::Result<usize> {
        let fd = self
            .mem
            .get_or_init(|| self.open_mem())
            .as_ref()
            .map_err(|&errno| io::Error::from_raw_os_error(errno))?;
        let at =
            libc::off_t::try_from(addr).map_err(|_| io::Error::from_raw_os_error(libc::EFAULT))?;
        // SAFETY: `buf` is valid for writes of its length.
        let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), at) };
        let errno = (n < 0).then(|| io::Error::last_os_error().raw_os_error());
        pread_result(n, errno.flatten())
    }
}

/// Maps a `/proc/<pid>/mem` open error to a memory-read error. Never returns
/// a path-lookup error recognized by [`kernel_fails`], which would let the
/// caller's syscall through unchecked.
fn open_errno(errno: Option<i32>) -> i32 {
    match errno {
        Some(libc::EACCES | libc::EPERM) => libc::EPERM,
        // No `/proc/<pid>`: the caller is gone.
        Some(libc::ENOENT | libc::ESRCH) => libc::ESRCH,
        Some(e) if !kernel_fails(&io::Error::from_raw_os_error(e)) => e,
        _ => libc::EIO,
    }
}

/// What `pread` on `/proc/<pid>/mem` returning `n`, failing with `errno`,
/// means, as `process_vm_readv` has it.
fn pread_result(n: isize, errno: Option<i32>) -> io::Result<usize> {
    match usize::try_from(n) {
        Ok(n) if n > 0 => Ok(n),
        // The caller's memory is gone: it exited or exec'd.
        Ok(_) => Err(io::Error::from_raw_os_error(libc::ESRCH)),
        Err(_) => Err(io::Error::from_raw_os_error(match errno {
            // Nothing mapped there, `process_vm_readv`'s `EFAULT`.
            Some(libc::EIO) => libc::EFAULT,
            Some(e) if !kernel_fails(&io::Error::from_raw_os_error(e)) => e,
            _ => libc::EIO,
        })),
    }
}

impl Memory for Process<'_> {
    fn read(&self, addr: u64, buf: &mut [u8]) -> io::Result<usize> {
        if !no_vm_readv() {
            match self.read_vm(addr, buf) {
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                    NO_VM_READV.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                read => return read,
            }
        }
        self.read_proc(addr, buf)
    }
}

/// The thread a syscall came from: `pid` names it in `/proc`, `mem` reads its
/// memory.
struct Caller<'a> {
    pid: u32,
    mem: &'a dyn Memory,
}

impl Caller<'_> {
    /// Opens `/proc/<pid>/<what>` (`O_PATH`), following a final link there.
    fn open_proc(&self, what: &str) -> io::Result<OwnedFd> {
        let path = CString::new(format!("/proc/{}/{what}", self.pid))
            .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: `path` is NUL-terminated and outlives the call.
        owned(unsafe { libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) }).map_err(|e| {
            // Following the link failed the ptrace access check: report it
            // as such, with what it means, rather than as a kernel failure.
            if e.raw_os_error() == Some(libc::EACCES) {
                io::Error::from_raw_os_error(libc::EPERM)
            } else {
                e
            }
        })
    }

    /// The caller's process id: the thread group of the thread `pid`.
    fn tgid(&self) -> io::Result<u32> {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.pid))?;
        status
            .lines()
            .find_map(|l| l.strip_prefix("Tgid:"))
            .and_then(|v| v.trim().parse().ok())
            .ok_or_else(|| io::Error::other("no Tgid in /proc/<pid>/status"))
    }
}

/// What a path names for the caller.
enum Found {
    /// An existing entry, opened `O_PATH`.
    Entry(OwnedFd),
    /// A missing last component: the directory it would be in.
    Missing(OwnedFd),
    /// Nothing: the kernel fails the lookup before checking any permission.
    Fails,
}

/// How to look a path argument up.
#[derive(Clone, Copy, Default)]
struct Lookup {
    /// Follow a symlink in the last component.
    follow: bool,
    /// An empty path names the directory descriptor itself (`AT_EMPTY_PATH`).
    empty: bool,
    /// The directory descriptor is the root (`RESOLVE_IN_ROOT`).
    in_root: bool,
    /// The path names an entry to create, remove, link into or rename: one
    /// ending in `.` or `..`, or the root itself, fails in the kernel
    /// (`EEXIST`, `EINVAL`, `ENOTEMPTY`, `EBUSY`, ...) before any permission
    /// is checked.
    entry: bool,
    /// `openat2`'s `RESOLVE_BENEATH`: escaping the directory descriptor fails.
    beneath: bool,
    /// `RESOLVE_NO_XDEV`: crossing a mount fails.
    no_xdev: bool,
    /// `RESOLVE_NO_SYMLINKS`: following any symlink fails.
    no_symlinks: bool,
    /// `RESOLVE_NO_MAGICLINKS`: following a `/proc/<pid>` link fails.
    no_magiclinks: bool,
}

impl Lookup {
    /// The lookup of an entry the syscall creates, removes or renames.
    fn entry() -> Self {
        Self {
            entry: true,
            ..Self::default()
        }
    }
}

fn arg(args: &[u64; 6], i: usize) -> u64 {
    args.get(i).copied().unwrap_or(0)
}

/// The accesses a notified syscall asks for.
fn accesses(c: &Caller<'_>, args: &[u64; 6], op: Op) -> Result<Vec<Check>, Unchecked> {
    let flag = |i: Option<usize>| i.map_or(0, |i| arg(args, i) as i32);
    let mut out = Vec::new();
    let mut need = |path: PathBuf, rights: u64, what: &'static str| {
        out.push(Check::Access { path, rights, what });
    };
    match op {
        Op::Open { at, flags } => {
            let (flags, resolve) = match flags {
                Flags::Arg(i) => (arg(args, i) as i32, 0),
                Flags::How(i) => {
                    // `struct open_how { u64 flags, mode, resolve; }`
                    let mut how = [0u8; 24];
                    match read_exact(c.mem, arg(args, i), &mut how) {
                        Ok(()) => {}
                        Err(e) if kernel_fails(&e) => return Ok(Vec::new()),
                        Err(e) => return Err(e.into()),
                    }
                    let word = |at: usize| {
                        how.get(at..at.saturating_add(8))
                            .and_then(|b| <[u8; 8]>::try_from(b).ok())
                            .map_or(0, u64::from_ne_bytes)
                    };
                    (word(0) as i32, word(16))
                }
                Flags::Creat => (libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC, 0),
            };
            const KNOWN: u64 = RESOLVE_NO_XDEV
                | RESOLVE_NO_MAGICLINKS
                | RESOLVE_NO_SYMLINKS
                | RESOLVE_BENEATH
                | RESOLVE_IN_ROOT
                | RESOLVE_CACHED;
            if resolve & !KNOWN != 0 {
                return unknown_resolve(resolve & !KNOWN, kernel_accepts_resolve)
                    .map(|()| Vec::new());
            }
            // `RESOLVE_CACHED` opens what is in the dentry cache, checked as
            // any other open; with these flags the kernel fails it with
            // `EAGAIN` up front.
            if resolve & RESOLVE_CACHED != 0
                && flags & (libc::O_CREAT | libc::O_TRUNC | (libc::O_TMPFILE & !libc::O_DIRECTORY))
                    != 0
            {
                return Ok(Vec::new());
            }
            // Landlock does not check `O_PATH` either: such a descriptor reads
            // nothing, and reopening it through `/proc` is an open of its own.
            if flags & libc::O_PATH != 0 {
                return Ok(Vec::new());
            }
            let create = flags & libc::O_CREAT != 0;
            let exclusive = create && flags & libc::O_EXCL != 0;
            // The other resolve flags only make the lookup fail where it
            // would otherwise succeed: those failures are the kernel's, before
            // any permission is checked.
            let how = Lookup {
                follow: flags & libc::O_NOFOLLOW == 0 && !exclusive,
                in_root: resolve & RESOLVE_IN_ROOT != 0,
                beneath: resolve & RESOLVE_BENEATH != 0,
                no_xdev: resolve & RESOLVE_NO_XDEV != 0,
                no_symlinks: resolve & RESOLVE_NO_SYMLINKS != 0,
                // Confined lookups refuse magic links too (`EXDEV`).
                no_magiclinks: resolve
                    & (RESOLVE_NO_MAGICLINKS
                        | RESOLVE_NO_SYMLINKS
                        | RESOLVE_BENEATH
                        | RESOLVE_IN_ROOT)
                    != 0,
                ..Lookup::default()
            };
            match find(c, args, at, how)? {
                Found::Entry(_) if exclusive => {}
                Found::Entry(fd) => {
                    let path = path_of(&fd)?;
                    let kind = file_type(&fd)?;
                    // The kernel fails these before checking any permission:
                    // a final symlink not followed (`O_NOFOLLOW`: `ELOOP`), and
                    // `O_DIRECTORY` on anything else (`ENOTDIR`).
                    if kind == libc::S_IFLNK
                        || (flags & libc::O_DIRECTORY != 0 && kind != libc::S_IFDIR)
                    {
                        return Ok(Vec::new());
                    }
                    if kind == libc::S_IFDIR {
                        if flags & libc::O_TMPFILE == libc::O_TMPFILE {
                            // An unnamed file in the directory, opened for
                            // writing: Landlock checks the file rights the
                            // directory grants.
                            if flags & libc::O_ACCMODE == libc::O_RDWR {
                                need(path.clone(), access::READ_FILE, "create");
                            }
                            need(path, access::WRITE_FILE, "create");
                        } else if flags & libc::O_ACCMODE != libc::O_RDONLY
                            || flags & (libc::O_CREAT | libc::O_TRUNC) != 0
                        {
                            // Opening a directory to write, or with
                            // `O_CREAT`/`O_TRUNC`: `EISDIR`, before any
                            // permission is checked.
                        } else {
                            need(path, access::READ_DIR, "list");
                        }
                    } else {
                        let mode = flags & libc::O_ACCMODE;
                        if mode == libc::O_RDONLY || mode == libc::O_RDWR {
                            need(path.clone(), access::READ_FILE, "read");
                        }
                        if mode == libc::O_WRONLY
                            || mode == libc::O_RDWR
                            || flags & libc::O_TRUNC != 0
                        {
                            need(path, access::WRITE_FILE, "write");
                        }
                    }
                }
                Found::Missing(dir) if create => need(path_of(&dir)?, access::MAKE_REG, "create"),
                Found::Missing(_) | Found::Fails => {}
            }
        }
        Op::Exec { at, flags } => {
            let flags = flag(flags);
            let how = Lookup {
                follow: flags & libc::AT_SYMLINK_NOFOLLOW == 0,
                empty: flags & libc::AT_EMPTY_PATH != 0,
                ..Lookup::default()
            };
            // A final symlink not followed (`AT_SYMLINK_NOFOLLOW`) fails with
            // `ELOOP` before any permission is checked.
            if let Found::Entry(fd) = find(c, args, at, how)?
                && file_type(&fd)? != libc::S_IFLNK
            {
                need(path_of(&fd)?, access::EXECUTE, "execute");
            }
        }
        // An entry that already exists fails before the kernel checks any
        // permission: skip it, as Landlock never sees it.
        Op::Make { at, kind } => {
            if let Found::Missing(dir) = find(c, args, at, Lookup::entry())? {
                let right = match kind {
                    Kind::Right(right) => right,
                    Kind::Mode(i) => make_right(arg(args, i) as libc::mode_t),
                };
                need(path_of(&dir)?, right, "create");
            }
        }
        Op::Link { old, new, flags } => {
            let flags = flag(flags);
            let how = Lookup {
                follow: flags & libc::AT_SYMLINK_FOLLOW != 0,
                empty: flags & libc::AT_EMPTY_PATH != 0,
                ..Lookup::default()
            };
            let Found::Entry(src) = find(c, args, old, how)? else {
                return Ok(out);
            };
            let Found::Missing(dst_dir) = find(c, args, new, Lookup::entry())? else {
                return Ok(out);
            };
            let entry = path_of(&src)?;
            let (from, to) = (parent(&entry), path_of(&dst_dir)?);
            need(to.clone(), make_right(file_type(&src)?), "create");
            out.push(Check::Refer {
                entry,
                from,
                to,
                dir: false,
                what: "link",
            });
        }
        Op::Remove { at, flags } => {
            if let Found::Entry(fd) = find(c, args, at, Lookup::entry())? {
                let removes_dir = match flags {
                    Some(i) => arg(args, i) as i32 & libc::AT_REMOVEDIR != 0,
                    None => file_type(&fd)? == libc::S_IFDIR,
                };
                let right = if removes_dir {
                    access::REMOVE_DIR
                } else {
                    access::REMOVE_FILE
                };
                need(parent(&path_of(&fd)?), right, "remove");
            }
        }
        Op::Rename { old, new, flags } => {
            let exchange = flag(flags) as u32 & libc::RENAME_EXCHANGE != 0;
            let noreplace = flag(flags) as u32 & libc::RENAME_NOREPLACE != 0;
            let Found::Entry(src) = find(c, args, old, Lookup::entry())? else {
                return Ok(out);
            };
            let (dst, to) = match find(c, args, new, Lookup::entry())? {
                // The kernel fails these before checking any permission:
                // replacing an entry with `RENAME_NOREPLACE` (`EEXIST`), and
                // exchanging with a missing one (`ENOENT`).
                Found::Entry(_) if noreplace => return Ok(out),
                Found::Missing(_) if exchange => return Ok(out),
                Found::Entry(fd) => {
                    let to = parent(&path_of(&fd)?);
                    (Some(fd), to)
                }
                Found::Missing(dir) => (None, path_of(&dir)?),
                Found::Fails => return Ok(out),
            };
            let entry = path_of(&src)?;
            let from = parent(&entry);
            let kind = file_type(&src)?;
            need(from.clone(), remove_right(kind), "rename");
            need(to.clone(), make_right(kind), "rename");
            let mut moves = vec![(entry, from.clone(), to.clone(), kind)];
            if let Some(dst) = dst {
                let kind = file_type(&dst)?;
                need(to.clone(), remove_right(kind), "rename");
                if exchange {
                    need(from.clone(), make_right(kind), "rename");
                    moves.push((path_of(&dst)?, to, from, kind));
                }
            }
            for (entry, from, to, kind) in moves {
                out.push(Check::Refer {
                    entry,
                    from,
                    to,
                    dir: kind == libc::S_IFDIR,
                    what: "move",
                });
            }
        }
        Op::Truncate { at } => {
            let how = Lookup {
                follow: true,
                ..Lookup::default()
            };
            if let Found::Entry(fd) = find(c, args, at, how)? {
                need(path_of(&fd)?, access::TRUNCATE, "write");
            }
        }
    }
    Ok(out)
}

/// The verdict on `openat2` resolve flags this code does not know: a kernel
/// that rejects them fails the syscall with `EINVAL` before any lookup, which
/// a program probing for a newer flag relies on; one that accepts them may
/// widen what the lookup reaches, which cannot be checked here.
fn unknown_resolve(bits: u64, accepts: impl FnOnce(u64) -> bool) -> Result<(), Unchecked> {
    if accepts(bits) {
        Err(Unchecked(format!(
            "unknown openat2 resolve flags {bits:#x}"
        )))
    } else {
        Ok(())
    }
}

/// Whether the running kernel accepts the resolve flags `bits`, tried once
/// per value with an `openat2` of `/` here: anything but `EINVAL` counts as
/// accepting them.
fn kernel_accepts_resolve(bits: u64) -> bool {
    static TRIED: std::sync::Mutex<Vec<(u64, bool)>> = std::sync::Mutex::new(Vec::new());
    let mut tried = TRIED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(&(_, accepts)) = tried.iter().find(|(b, _)| *b == bits) {
        return accepts;
    }
    let how: [u64; 3] = [(libc::O_PATH | libc::O_CLOEXEC) as u64, 0, bits];
    // SAFETY: the path is NUL-terminated and static, and `how` is a valid
    // `open_how` of the size passed.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            libc::AT_FDCWD,
            c"/".as_ptr(),
            how.as_ptr(),
            std::mem::size_of_val(&how),
        )
    };
    let accepts = if fd < 0 {
        io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL)
    } else {
        // SAFETY: the kernel just returned this descriptor.
        drop(i32::try_from(fd).map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }));
        true
    };
    tried.push((bits, accepts));
    accepts
}

/// Looks the path argument `at` up the way the kernel will for the caller.
fn find(c: &Caller<'_>, args: &[u64; 6], at: At, how: Lookup) -> Result<Found, Unchecked> {
    let path = match read_cstr(c.mem, arg(args, at.1)) {
        Ok(path) => path,
        Err(e) if kernel_fails(&e) => return Ok(Found::Fails),
        Err(e) => return Err(e.into()),
    };
    if how.entry && names_no_entry(&path) {
        return Ok(Found::Fails);
    }
    let relative = !path.starts_with(b"/");
    // An absolute path escapes the directory descriptor: `EXDEV`.
    if how.beneath && !relative {
        return Ok(Found::Fails);
    }
    let base = if relative || how.in_root || how.beneath {
        let dirfd = at.0.map_or(libc::AT_FDCWD, |i| arg(args, i) as i32);
        let what = if dirfd == libc::AT_FDCWD {
            "cwd".to_string()
        } else {
            format!("fd/{dirfd}")
        };
        match c.open_proc(&what) {
            Ok(fd) => Some(fd),
            // Not an open descriptor: the caller gets `EBADF`.
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Ok(Found::Fails),
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    if path.is_empty() {
        return Ok(match base {
            Some(fd) if how.empty => Found::Entry(fd),
            _ => Found::Fails,
        });
    }
    let root = match &base {
        Some(fd) if how.in_root || how.beneath => fd.try_clone()?,
        _ => c.open_proc("root")?,
    };
    let start = match base {
        Some(fd) if relative => fd,
        _ => root.try_clone()?,
    };
    walk(c, &root, start, &path, how)
}

/// Whether `path` ends in `.` or `..`, or is the root: it names no entry of
/// its own that could be created, removed or renamed.
fn names_no_entry(path: &[u8]) -> bool {
    match path.split(|&b| b == b'/').rfind(|c| !c.is_empty()) {
        None => true,
        Some(name) => name == b"." || name == b"..",
    }
}

/// The mount `fd` is on, from `/proc/self/fdinfo`.
fn mount_id(fd: &OwnedFd) -> io::Result<u64> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd()))?;
    info.lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| io::Error::other("no mnt_id in /proc/self/fdinfo"))
}

/// Resolves `path` from `start` one component at a time, with `root` as `/`
/// (under `RESOLVE_BENEATH`, the directory not to escape).
fn walk(
    c: &Caller<'_>,
    root: &OwnedFd,
    start: OwnedFd,
    path: &[u8],
    how: Lookup,
) -> Result<Found, Unchecked> {
    let follow = how.follow || path.ends_with(b"/");
    let root_id = id(root)?;
    let mount = if how.no_xdev {
        Some(mount_id(&start)?)
    } else {
        None
    };
    // Under `RESOLVE_NO_XDEV`, whether `fd` is on another mount: `EXDEV`.
    let crosses = |fd: &OwnedFd| -> io::Result<bool> {
        Ok(match mount {
            Some(m) => mount_id(fd)? != m,
            None => false,
        })
    };
    let mut cur = start;
    let mut todo = Vec::new();
    push_components(&mut todo, path);
    let mut links = 0u32;
    while let Some(name) = todo.pop() {
        let last = todo.is_empty();
        if name == b".." {
            if id(&cur)? != root_id {
                cur = open_at(&cur, b"..", false)?;
                if crosses(&cur)? {
                    return Ok(Found::Fails);
                }
            } else if how.beneath {
                return Ok(Found::Fails);
            }
            continue;
        }
        let proc = is_proc(&cur)?;
        let proc_root = proc && id(&cur)?.1 == PROC_ROOT_INO;
        if proc_root {
            let alias = match name.as_slice() {
                b"self" => Some(c.tgid()?.to_string()),
                b"thread-self" => Some(format!("{}/task/{}", c.tgid()?, c.pid)),
                _ => None,
            };
            if let Some(alias) = alias {
                push_components(&mut todo, alias.as_bytes());
                continue;
            }
        }
        let fd = match open_at(&cur, &name, false) {
            Ok(fd) => fd,
            Err(e) if last && e.raw_os_error() == Some(libc::ENOENT) => {
                return Ok(Found::Missing(cur));
            }
            Err(e) if kernel_fails(&e) => return Ok(Found::Fails),
            Err(e) => return Err(e.into()),
        };
        if crosses(&fd)? {
            return Ok(Found::Fails);
        }
        if (follow || !last) && file_type(&fd)? == libc::S_IFLNK {
            links = links.saturating_add(1);
            if links > MAX_LINKS || how.no_symlinks {
                return Ok(Found::Fails);
            }
            if proc && !proc_root {
                if how.no_magiclinks {
                    return Ok(Found::Fails);
                }
                // A link under /proc/<pid> (`fd/N`, `cwd`, `root`, ...): the
                // kernel follows it to the caller's object.
                cur = match open_at(&cur, &name, true) {
                    Ok(fd) => fd,
                    Err(e) if kernel_fails(&e) => return Ok(Found::Fails),
                    Err(e) => return Err(e.into()),
                };
                if crosses(&cur)? {
                    return Ok(Found::Fails);
                }
                continue;
            }
            let target = read_link_at(&cur, &name)?;
            if target.starts_with(b"/") {
                if how.beneath {
                    return Ok(Found::Fails);
                }
                cur = root.try_clone()?;
                if crosses(&cur)? {
                    return Ok(Found::Fails);
                }
            }
            push_components(&mut todo, &target);
            continue;
        }
        if last {
            return Ok(Found::Entry(fd));
        }
        cur = fd;
    }
    Ok(Found::Entry(cur))
}

/// Pushes the components of `path` onto `todo`, last first, without `.`.
fn push_components(todo: &mut Vec<Vec<u8>>, path: &[u8]) {
    for name in path.rsplit(|&b| b == b'/') {
        if !name.is_empty() && name != b"." {
            todo.push(name.to_vec());
        }
    }
}

/// Errors with which the kernel fails the syscall itself, before any
/// permission the sandbox adds is checked.
fn kernel_fails(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::ENOENT
                | libc::ENOTDIR
                | libc::ELOOP
                | libc::ENAMETOOLONG
                | libc::EACCES
                | libc::EFAULT
                | libc::EBADF
        )
    )
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned this descriptor and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Opens `name` in `dir` with `O_PATH`.
fn open_at(dir: &OwnedFd, name: &[u8], follow: bool) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
    let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
    // SAFETY: `name` is NUL-terminated and outlives the call.
    owned(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_CLOEXEC | nofollow,
        )
    })
}

fn read_link_at(dir: &OwnedFd, name: &[u8]) -> io::Result<Vec<u8>> {
    let name = CString::new(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: `buf` has room for the `buf.len()` bytes the kernel may write.
    let n = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let n = usize::try_from(n).map_err(|_| io::Error::last_os_error())?;
    buf.truncate(n);
    Ok(buf)
}

fn stat(fd: &OwnedFd) -> io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is valid, and the kernel fills it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `stat` for the kernel to fill.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// The `S_IF*` file type of `fd`.
fn file_type(fd: &OwnedFd) -> io::Result<libc::mode_t> {
    Ok(stat(fd)?.st_mode & libc::S_IFMT)
}

/// The device and inode of `fd`.
fn id(fd: &OwnedFd) -> io::Result<(u64, u64)> {
    let st = stat(fd)?;
    Ok((st.st_dev, st.st_ino))
}

fn is_proc(fd: &OwnedFd) -> io::Result<bool> {
    // SAFETY: an all-zero `statfs` is valid, and the kernel fills it.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is a valid `statfs` for the kernel to fill.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // `f_type` is signed on glibc and unsigned on musl.
    Ok(i128::from(st.f_type) == i128::from(libc::PROC_SUPER_MAGIC))
}

/// Where `fd` is, as this process sees it.
fn path_of(fd: &OwnedFd) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
}

fn parent(path: &Path) -> PathBuf {
    path.parent()
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

/// The right to create an entry of file type `kind` (`S_IF*`).
fn make_right(kind: libc::mode_t) -> u64 {
    match kind & libc::S_IFMT {
        libc::S_IFDIR => access::MAKE_DIR,
        libc::S_IFLNK => access::MAKE_SYM,
        libc::S_IFCHR => access::MAKE_CHAR,
        libc::S_IFBLK => access::MAKE_BLOCK,
        libc::S_IFIFO => access::MAKE_FIFO,
        libc::S_IFSOCK => access::MAKE_SOCK,
        _ => access::MAKE_REG,
    }
}

fn remove_right(kind: libc::mode_t) -> u64 {
    if kind == libc::S_IFDIR {
        access::REMOVE_DIR
    } else {
        access::REMOVE_FILE
    }
}

/// Reads a NUL-terminated string of at most `PATH_MAX` bytes, a page at a time
/// so that the read never runs into an unmapped page.
fn read_cstr(mem: &dyn Memory, addr: u64) -> io::Result<Vec<u8>> {
    const PAGE: u64 = 4096;
    const OFFSET_MASK: u64 = PAGE - 1;
    let mut out = Vec::new();
    let mut at = addr;
    while out.len() < libc::PATH_MAX as usize {
        let chunk = PAGE.saturating_sub(at & OFFSET_MASK);
        let mut buf = vec![0u8; usize::try_from(chunk).unwrap_or(0)];
        let n = mem.read(at, &mut buf)?;
        buf.truncate(n);
        if let Some(end) = buf.iter().position(|&b| b == 0) {
            buf.truncate(end);
            out.extend_from_slice(&buf);
            return Ok(out);
        }
        out.extend_from_slice(&buf);
        at = at
            .checked_add(chunk)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::EFAULT))?;
    }
    Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG))
}

fn read_exact(mem: &dyn Memory, addr: u64, buf: &mut [u8]) -> io::Result<()> {
    if mem.read(addr, buf)? == buf.len() {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(libc::EFAULT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;

    #[test]
    fn program_jumps_land_on_the_notify() {
        let ops = ops();
        let prog = program(&ops).unwrap();
        let notify = prog.len() - 1;
        for (i, ins) in prog.iter().enumerate().skip(4).take(ops.len() + 1) {
            assert_eq!(i + 1 + ins.jt as usize, notify);
        }
        assert_eq!(prog[notify].k, libc::SECCOMP_RET_USER_NOTIF);
        // Another architecture's syscalls are handed over too.
        assert_eq!(prog[2].k, libc::SECCOMP_RET_USER_NOTIF);
    }

    /// Memory that cannot be read, as for a caller that is not dumpable.
    struct Unreadable;

    impl Memory for Unreadable {
        fn read(&self, _: u64, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        }
    }

    fn openat(path: &CString, flags: i32) -> (Op, [u64; 6]) {
        let (_, _, op) = ops().into_iter().find(|(_, n, _)| *n == "openat").unwrap();
        let args = [
            libc::AT_FDCWD as u64,
            path.as_ptr() as u64,
            flags as u64,
            0,
            0,
            0,
        ];
        (op, args)
    }

    fn me() -> Process<'static> {
        Process::new(std::process::id())
    }

    /// A fresh directory with `secret/key`, `src/a` and `out/`, canonical.
    fn tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("task-seccomp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["secret/sub", "src", "out"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("secret/key"), "k").unwrap();
        std::fs::write(root.join("src/a"), "a").unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    fn cstr(p: &Path) -> CString {
        CString::new(p.as_os_str().as_bytes()).unwrap()
    }

    fn read_of(path: PathBuf) -> Check {
        Check::Access {
            path,
            rights: access::READ_FILE,
            what: "read",
        }
    }

    #[test]
    fn memory_is_read_through_proc_without_process_vm_readv() {
        static BUF: [u8; 6] = *b"path\0x";
        // From another thread, by its thread id, as a notification names it.
        let (tx, rx) = std::sync::mpsc::channel();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            // SAFETY: gettid takes no arguments and cannot fail.
            tx.send(unsafe { libc::syscall(libc::SYS_gettid) } as u32)
                .unwrap();
            wait.recv().unwrap();
        });
        let tid = rx.recv().unwrap();
        let mem = Process::new(tid);
        let mut buf = [0u8; 6];
        assert_eq!(mem.read_proc(BUF.as_ptr() as u64, &mut buf).unwrap(), 6);
        assert_eq!(&buf, &BUF);
        let unmapped = mem.read_proc(0, &mut buf).unwrap_err();
        assert_eq!(unmapped.raw_os_error(), Some(libc::EFAULT));
        done.send(()).unwrap();
        thread.join().unwrap();
        // Read once the notification is gone: what the pid names may be
        // another process.
        let listener = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let gone = Process::notified(std::process::id(), &listener, 1);
        let e = gone.read_proc(BUF.as_ptr() as u64, &mut buf).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ESRCH));
        // No such process (above any `pid_max`): gone too, not a lookup
        // failure that would let the syscall through unchecked.
        let e = Process::new(1 << 30).read_proc(0, &mut buf).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn proc_mem_failures_never_read_as_a_failed_lookup() {
        let errno = |r: io::Result<usize>| r.unwrap_err().raw_os_error().unwrap();
        assert_eq!(open_errno(Some(libc::EACCES)), libc::EPERM);
        assert_eq!(open_errno(Some(libc::ENOENT)), libc::ESRCH);
        assert_eq!(open_errno(Some(libc::ENOTDIR)), libc::EIO);
        assert_eq!(open_errno(Some(libc::EMFILE)), libc::EMFILE);
        assert_eq!(open_errno(None), libc::EIO);
        assert_eq!(pread_result(4, None).unwrap(), 4);
        assert_eq!(errno(pread_result(0, None)), libc::ESRCH);
        assert_eq!(errno(pread_result(-1, Some(libc::EIO))), libc::EFAULT);
        assert_eq!(errno(pread_result(-1, Some(libc::EFAULT))), libc::EIO);
        assert_eq!(errno(pread_result(-1, Some(libc::EINTR))), libc::EINTR);
    }

    #[test]
    fn an_unreadable_caller_is_unchecked_and_denied() {
        let path = CString::new("/etc/hostname").unwrap();
        let (op, args) = openat(&path, libc::O_RDONLY);
        let caller = Caller {
            pid: std::process::id(),
            mem: &Unreadable,
        };
        let checks = accesses(&caller, &args, op);
        assert!(checks.is_err());
        let denials = judge(&[], "openat", checks);
        let [Denial::Unchecked { syscall, why }] = denials.as_slice() else {
            panic!("{denials:?}");
        };
        assert_eq!(*syscall, "openat");
        assert!(why.contains("not dumpable"), "{why}");
    }

    fn by_name(name: &str) -> Op {
        ops().into_iter().find(|(_, n, _)| *n == name).unwrap().2
    }

    #[test]
    fn an_entry_named_dot_or_dot_dot_checks_nothing() {
        let root = tree("dots");
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let at = libc::AT_FDCWD as u64;
        for p in ["secret/.", "secret/sub/..", "secret/sub/./", "/"] {
            let path = if p == "/" {
                CString::new("/").unwrap()
            } else {
                cstr(&root.join(p))
            };
            let ptr = path.as_ptr() as u64;
            // rmdir: `EINVAL`, `ENOTEMPTY` or `EBUSY`.
            let rmdir = [at, ptr, libc::AT_REMOVEDIR as u64, 0, 0, 0];
            assert_eq!(accesses(&caller, &rmdir, by_name("unlinkat")).unwrap(), []);
            // mkdir: `EEXIST`.
            let mkdir = [at, ptr, 0o755, 0, 0, 0];
            assert_eq!(accesses(&caller, &mkdir, by_name("mkdirat")).unwrap(), []);
            // rename either way: `EBUSY` or `EINVAL`.
            let other = cstr(&root.join("src/a"));
            let o = other.as_ptr() as u64;
            for (from, to) in [(ptr, o), (o, ptr)] {
                let rename = [at, from, at, to, 0, 0];
                assert_eq!(
                    accesses(&caller, &rename, by_name("renameat2")).unwrap(),
                    []
                );
            }
            // A link named so: `EEXIST`.
            let link = [at, o, at, ptr, 0, 0];
            assert_eq!(accesses(&caller, &link, by_name("linkat")).unwrap(), []);
        }
        // A plain name is still checked.
        let sub = cstr(&root.join("secret/sub"));
        let rmdir = [at, sub.as_ptr() as u64, libc::AT_REMOVEDIR as u64, 0, 0, 0];
        assert!(
            !accesses(&caller, &rmdir, by_name("unlinkat"))
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unnamed_temporary_file_needs_the_file_rights_of_its_directory() {
        let root = tree("tmpfile");
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let dir = cstr(&root.join("secret"));
        let (op, args) = openat(&dir, libc::O_TMPFILE | libc::O_RDWR);
        let checks = accesses(&caller, &args, op).unwrap();
        let path = root.join("secret");
        assert_eq!(
            checks,
            vec![
                Check::Access {
                    path: path.clone(),
                    rights: access::READ_FILE,
                    what: "create",
                },
                Check::Access {
                    path,
                    rights: access::WRITE_FILE,
                    what: "create",
                },
            ]
        );
        let (op, args) = openat(&dir, libc::O_TMPFILE | libc::O_WRONLY);
        assert_eq!(accesses(&caller, &args, op).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_flags_the_kernel_fails_first_check_nothing() {
        let root = tree("resolve");
        symlink(root.join("secret/key"), root.join("src/link")).unwrap();
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let open2 = |dir: &std::fs::File, path: &str, resolve: u64| {
            let path = CString::new(path).unwrap();
            let how: [u64; 3] = [libc::O_RDONLY as u64, 0, resolve];
            let args = [
                dir.as_raw_fd() as u64,
                path.as_ptr() as u64,
                how.as_ptr() as u64,
                24,
                0,
                0,
            ];
            accesses(&caller, &args, by_name("openat2")).unwrap()
        };
        let src = std::fs::File::open(root.join("src")).unwrap();
        let key = vec![read_of(root.join("secret/key"))];
        // Escaping the directory: `EXDEV`.
        assert_eq!(open2(&src, "../secret/key", 0), key);
        assert_eq!(open2(&src, "../secret/key", RESOLVE_BENEATH), []);
        let abs = root.join("secret/key");
        assert_eq!(open2(&src, abs.to_str().unwrap(), RESOLVE_BENEATH), []);
        assert_eq!(open2(&src, "link", RESOLVE_BENEATH), []);
        // A symlink: `ELOOP`.
        assert_eq!(open2(&src, "link", 0), key);
        assert_eq!(open2(&src, "link", RESOLVE_NO_SYMLINKS), []);
        // Into another mount: `EXDEV`.
        let slash = std::fs::File::open("/").unwrap();
        let version = vec![read_of(PathBuf::from("/proc/version"))];
        assert_eq!(open2(&slash, "proc/version", 0), version);
        assert_eq!(open2(&slash, "proc/version", RESOLVE_NO_XDEV), []);
        // A `/proc/<pid>` link: `ELOOP`.
        let fd = format!("proc/self/fd/{}", src.as_raw_fd());
        assert_eq!(
            open2(&slash, &format!("{fd}/a"), 0),
            [read_of(root.join("src/a"))]
        );
        assert_eq!(open2(&slash, &format!("{fd}/a"), RESOLVE_NO_MAGICLINKS), []);
        // Confined lookups refuse magic links: `EXDEV`.
        assert_eq!(open2(&slash, &format!("{fd}/a"), RESOLVE_BENEATH), []);
        assert_eq!(open2(&slash, &format!("{fd}/a"), RESOLVE_IN_ROOT), []);
        // A cached lookup opens what an uncached one would: checked.
        assert_eq!(open2(&src, "../secret/key", RESOLVE_CACHED), key);
        // Unless creating or truncating, which it fails with `EAGAIN`.
        let path = CString::new("../secret/key").unwrap();
        let open_how = |flags: i32, resolve: u64| {
            let how: [u64; 3] = [flags as u64, 0, resolve];
            let args = [
                src.as_raw_fd() as u64,
                path.as_ptr() as u64,
                how.as_ptr() as u64,
                24,
                0,
                0,
            ];
            accesses(&caller, &args, by_name("openat2"))
        };
        let trunc = libc::O_WRONLY | libc::O_TRUNC;
        assert_eq!(open_how(trunc, RESOLVE_CACHED).unwrap(), []);
        assert!(!open_how(trunc, 0).unwrap().is_empty());
        // A flag neither this code nor the kernel knows: `EINVAL`.
        assert_eq!(open_how(libc::O_RDONLY, 1 << 40).unwrap(), []);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unknown_resolve_flags_are_unchecked_only_where_the_kernel_takes_them() {
        assert!(unknown_resolve(1 << 40, |_| false).is_ok());
        let err = unknown_resolve(1 << 40, |_| true).unwrap_err();
        assert!(
            err.0.contains("unknown openat2 resolve flags 0x"),
            "{err:?}"
        );
        assert!(!kernel_accepts_resolve(1 << 40));
        // Cached: asked again, the answer is the same.
        assert!(!kernel_accepts_resolve(1 << 40));
        assert!(kernel_accepts_resolve(RESOLVE_BENEATH));
    }

    #[test]
    fn opening_a_directory_to_write_checks_nothing() {
        let root = tree("dirwrite");
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let dir = cstr(&root.join("secret"));
        // `EISDIR`, before any permission is checked.
        for flags in [
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_RDONLY | libc::O_CREAT,
            libc::O_RDONLY | libc::O_TRUNC,
        ] {
            let (op, args) = openat(&dir, flags);
            assert_eq!(accesses(&caller, &args, op).unwrap(), [], "{flags:#o}");
        }
        let (op, args) = openat(&dir, libc::O_RDONLY);
        let list = Check::Access {
            path: root.join("secret"),
            rights: access::READ_DIR,
            what: "list",
        };
        assert_eq!(accesses(&caller, &args, op).unwrap(), [list]);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Runs an x32 syscall on a thread under the filter, supervised with
    /// `enforce`: its result, and what the supervisor recorded.
    #[cfg(target_arch = "x86_64")]
    fn x32_getpid(enforce: bool) -> (i64, Option<i32>, crate::sandbox::Seen) {
        let denials = Denials::default();
        let (tx, rx) = std::sync::mpsc::channel::<OwnedFd>();
        let recorded = Arc::clone(&denials);
        let supervisor = std::thread::spawn(move || {
            if let Ok(listener) = rx.recv() {
                supervise(listener, Vec::new(), enforce, recorded);
            }
        });
        let (ret, errno) = std::thread::spawn(move || {
            let listener = install().unwrap();
            tx.send(listener).unwrap();
            let nr = libc::SYS_getpid | libc::c_long::from(X32_SYSCALL_BIT);
            // SAFETY: getpid takes no arguments.
            let ret = unsafe { libc::syscall(nr) };
            (ret, io::Error::last_os_error().raw_os_error())
        })
        .join()
        .unwrap();
        // The filtered thread is gone: the supervisor sees the hangup.
        supervisor.join().unwrap();
        let seen = std::mem::take(&mut *denials.lock().unwrap());
        (ret, errno, seen)
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn an_x32_syscall_is_noted_once_and_failed_when_enforcing() {
        if let Err(e) = probe() {
            assert!(
                std::env::var_os("TASK_TEST_SANDBOX_REQUIRED").is_none(),
                "{e}"
            );
            eprintln!("skipping: seccomp unavailable here: {e}");
            return;
        }
        let x32 = Denial::Foreign {
            syscalls: "x32 syscalls".to_string(),
        };
        let (ret, errno, seen) = x32_getpid(true);
        assert_eq!((ret, errno), (-1, Some(libc::EACCES)));
        assert_eq!(seen.denials, std::slice::from_ref(&x32));
        assert_eq!(seen.count, 0, "a note is no denial");
        // Auditing, it is let through (and fails as the kernel has it, with
        // `ENOSYS` without x32 support), and noted all the same.
        let (_, errno, seen) = x32_getpid(false);
        assert_ne!(errno, Some(libc::EACCES));
        assert_eq!(seen.denials, [x32]);
        assert_eq!(seen.count, 0);
    }

    #[test]
    fn dot_dot_comes_after_the_symlink_before_it() {
        let root = tree("dotdot");
        symlink(root.join("secret/sub"), root.join("link")).unwrap();
        std::fs::write(root.join("key"), "decoy").unwrap();
        let path = cstr(&root.join("link/../key"));
        let (op, args) = openat(&path, libc::O_RDONLY);
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let checks = accesses(&caller, &args, op).unwrap();
        assert_eq!(checks, vec![read_of(root.join("secret/key"))]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_open_the_kernel_fails_first_checks_nothing() {
        let root = tree("kernelfails");
        symlink(root.join("secret/key"), root.join("link")).unwrap();
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        // `O_NOFOLLOW` on a symlink: `ELOOP`.
        let link = cstr(&root.join("link"));
        let (op, args) = openat(&link, libc::O_RDONLY | libc::O_NOFOLLOW);
        assert_eq!(accesses(&caller, &args, op).unwrap(), Vec::new());
        // `O_DIRECTORY` on a file: `ENOTDIR`.
        let key = cstr(&root.join("secret/key"));
        let (op, args) = openat(&key, libc::O_RDONLY | libc::O_DIRECTORY);
        assert_eq!(accesses(&caller, &args, op).unwrap(), Vec::new());
        // Followed, the link is read.
        let (op, args) = openat(&link, libc::O_RDONLY);
        let checks = accesses(&caller, &args, op).unwrap();
        assert_eq!(checks, vec![read_of(root.join("secret/key"))]);
        // `execveat(.., AT_SYMLINK_NOFOLLOW)` on a symlink: `ELOOP`.
        let (_, _, op) = ops()
            .into_iter()
            .find(|(_, n, _)| *n == "execveat")
            .unwrap();
        let at = libc::AT_FDCWD as u64;
        let flags = libc::AT_SYMLINK_NOFOLLOW as u64;
        let args = [at, link.as_ptr() as u64, 0, 0, flags, 0];
        assert_eq!(accesses(&caller, &args, op).unwrap(), Vec::new());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_rename_the_kernel_fails_first_checks_nothing() {
        let root = tree("renamefails");
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let (_, _, op) = ops()
            .into_iter()
            .find(|(_, n, _)| *n == "renameat2")
            .unwrap();
        let at = libc::AT_FDCWD as u64;
        let rename = |old: &Path, new: &Path, flags: u32| {
            let (old, new) = (cstr(old), cstr(new));
            let args = [
                at,
                old.as_ptr() as u64,
                at,
                new.as_ptr() as u64,
                u64::from(flags),
                0,
            ];
            accesses(&caller, &args, op).unwrap()
        };
        let (a, key) = (root.join("src/a"), root.join("secret/key"));
        // Onto an existing entry with `RENAME_NOREPLACE`: `EEXIST`.
        assert_eq!(rename(&a, &key, libc::RENAME_NOREPLACE), Vec::new());
        // Exchanging with a missing entry: `ENOENT`.
        let none = root.join("out/none");
        assert_eq!(rename(&a, &none, libc::RENAME_EXCHANGE), Vec::new());
        // Plainly, it replaces the key: checked.
        assert!(!rename(&a, &key, 0).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_probe_lets_a_syscall_through() {
        // A container may filter `seccomp` itself: that is no failure here.
        match probe() {
            Ok(()) => {}
            Err(e) if std::env::var_os("TASK_TEST_SANDBOX_REQUIRED").is_none() => {
                eprintln!("skipping: seccomp unavailable here: {e}");
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn a_repeated_denial_is_shown_once_but_counted_each_time() {
        let denials = Denials::default();
        let mut seen = HashSet::new();
        let d = || Denial::Unchecked {
            syscall: "openat",
            why: "x".to_string(),
        };
        record(&mut seen, &denials, d());
        let first = std::mem::take(&mut denials.lock().unwrap().denials);
        assert_eq!(first, vec![d()]);
        record(&mut seen, &denials, d());
        let again = denials.lock().unwrap();
        assert_eq!(again.count, 2, "a repeat must still fail its command");
        assert!(again.denials.is_empty());
    }

    #[test]
    fn proc_self_is_the_caller() {
        let root = tree("procself");
        let key = std::fs::File::open(root.join("secret/key")).unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(key)
            .spawn()
            .unwrap();
        let path = CString::new("/proc/self/fd/0").unwrap();
        let (op, args) = openat(&path, libc::O_RDONLY);
        // The path is in this process's memory; the caller is the child.
        let mem = me();
        let caller = Caller {
            pid: child.id(),
            mem: &mem,
        };
        let checks = accesses(&caller, &args, op);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(checks.unwrap(), vec![read_of(root.join("secret/key"))]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_in_root_starts_absolute_paths_at_the_directory() {
        let root = tree("inroot");
        let dir = std::fs::File::open(root.join("secret")).unwrap();
        let (_, _, op) = ops().into_iter().find(|(_, n, _)| *n == "openat2").unwrap();
        let path = CString::new("/../key").unwrap();
        let how: [u64; 3] = [libc::O_RDONLY as u64, 0, RESOLVE_IN_ROOT];
        let args = [
            dir.as_raw_fd() as u64,
            path.as_ptr() as u64,
            how.as_ptr() as u64,
            24,
            0,
            0,
        ];
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let checks = accesses(&caller, &args, op).unwrap();
        assert_eq!(checks, vec![read_of(root.join("secret/key"))]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_hard_link_checks_where_the_file_comes_from() {
        let root = tree("link");
        let (_, _, op) = ops().into_iter().find(|(_, n, _)| *n == "linkat").unwrap();
        let old = cstr(&root.join("secret/key"));
        let new = cstr(&root.join("out/key"));
        let at = libc::AT_FDCWD as u64;
        let args = [at, old.as_ptr() as u64, at, new.as_ptr() as u64, 0, 0];
        let mem = me();
        let caller = Caller {
            pid: std::process::id(),
            mem: &mem,
        };
        let checks = accesses(&caller, &args, op).unwrap();
        assert!(checks.contains(&Check::Refer {
            entry: root.join("secret/key"),
            from: root.join("secret"),
            to: root.join("out"),
            dir: false,
            what: "link",
        }));
        let _ = std::fs::remove_dir_all(&root);
    }
}

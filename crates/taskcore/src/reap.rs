//! Stopping the commands the engine started.
//!
//! A command runs inside an in-process shell interpreter, which spawns the
//! actual process as a `tokio::process::Child` it never hands back, and waits
//! for it. Abandoning the future that drives the interpreter — cancelling a
//! task, tearing down a failed run — therefore leaves that process running:
//! nothing in the interpreter's API hands back a handle to it, and dropping that
//! child reaps it without killing it. Stopping it has to start from the process
//! table instead.
//!
//! Two things about those processes shape what works here, both measured rather
//! than assumed:
//!
//! * They stay in *this* process's group, so signalling the group is not an
//!   option — it would take the engine down with them. Each process is signalled
//!   on its own, and a command's own children are reached by walking the process
//!   tree rather than by one group signal.
//! * A terminal's Ctrl-C already reaches them, precisely because they share the
//!   group. The sweep is therefore not how an interrupt gets delivered; it is for
//!   the commands nothing signalled at all — those abandoned part-way through a
//!   run. `SIGTERM` is the signal for that, with a `SIGKILL` for whatever ignores
//!   it.
//!
//! The sweep is deliberately blunt: it stops *every* command still running, with
//! no way to attribute one to the task that started it — a job a task
//! deliberately backgrounded is stopped along with the rest. Callers therefore
//! only use it while the whole run is being torn down, and `TASK_NO_REAP=1` in
//! the environment `task` runs in turns it off for a run that depends on such a
//! job surviving.
//!
//! A pid can in principle be recycled between the walk finding it and the signal
//! being sent, since these children are reaped as they exit. Linux could close
//! that window with `pidfd_open`/`pidfd_send_signal`; the other two platforms
//! have no equivalent, so the walk accepts it.
//!
//! The macOS and Windows arms below compile for their targets and have been
//! reviewed against their APIs, but have only been exercised on Linux. Neither
//! filters zombies either — Linux skips them, so there they neither inflate the
//! count nor buy a grace period nobody is waiting for.

/// How to stop a command.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// Ask it to terminate, leaving it a chance to clean up.
    Terminate,
    /// Stop it without giving it a say.
    Kill,
}

/// How long a command is given to act on [`Stop::Terminate`] before it is
/// killed.
///
/// Short on purpose: by the time the sweep runs the run is over and nobody is
/// reading the command's output, so this is only enough for a signal handler to
/// run, not for a compiler or a container to wind down.
#[cfg(not(windows))]
const GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Ceiling on how many processes the walk will report, and on how many parents
/// it will look under.
const MAX_WALK: usize = 4096;

/// What one sweep did, for the caller to report: it holds the logger, this
/// module does not.
#[derive(Default)]
pub(crate) struct Swept {
    /// How many processes were signalled to terminate.
    pub(crate) asked: usize,
    /// How many were still running once `GRACE` was up and were killed. Some of
    /// those were spawned during that window, or were exiting cleanly when it
    /// expired: this is what was killed, not what defied the terminate. Windows
    /// has no gentler first signal and no grace period, so there it is simply
    /// whatever the second walk still found.
    pub(crate) killed: usize,
    /// Processes that could not be signalled, and so may still be running.
    pub(crate) unstoppable: Vec<(i32, String)>,
    /// How many processes the walk had found when it stopped short, if it did —
    /// either at `MAX_WALK` or because a subtree could not be read. The count is
    /// what was found, not what was missed.
    pub(crate) truncated_at: Option<usize>,
    /// Whether the sweep declined to walk because this process adopts orphans
    /// (it is pid 1, or a child subreaper) and so cannot tell its own commands
    /// from a stranger's.
    pub(crate) declined: bool,
}

/// Whether the sweep has been turned off with `TASK_NO_REAP`.
///
/// The escape hatch exists because the sweep cannot tell a command abandoned
/// mid-flight from a job a task deliberately backgrounded, and a run that
/// depends on the latter needs Go's behavior of leaving everything running.
///
/// Read from the environment `task` was started in — a Taskfile's own `env:`
/// comes too late and does not reach this.
fn disabled() -> bool {
    crate::env::get_task_env_bool("NO_REAP").0
}

/// Stops every command still running: a terminate first, then a kill for
/// whatever is still there after `GRACE`.
///
/// The grace period is only waited out when there was something to wait for, so
/// a run that leaves nothing behind — the normal case — returns at once.
///
/// The walk and the signals block the calling thread. That is deliberate: by the
/// time this runs the run is over, so there is nothing else for the runtime to
/// be turning.
pub(crate) async fn stop_commands() -> Swept {
    let mut swept = Swept::default();
    if disabled() {
        return swept;
    }

    let walk = descendant_pids();
    swept.truncated_at = walk.truncated_at;
    swept.declined = walk.declined;
    swept.asked = signal_all(&walk.pids, Stop::Terminate, &mut swept.unstoppable);
    if swept.asked > 0 {
        // Windows terminates outright on the first pass — it has no gentler
        // signal to have waited for — so the grace period is pure latency there.
        #[cfg(not(windows))]
        tokio::time::sleep(GRACE).await;
        // Walked again rather than reusing the first list: what ignored the
        // terminate may have spawned children while doing so.
        let again = descendant_pids();
        swept.truncated_at = swept.truncated_at.or(again.truncated_at);
        swept.killed = signal_all(&again.pids, Stop::Kill, &mut swept.unstoppable);
    }
    // A process that refused both signals was recorded by each pass; it is one
    // survivor to report, not two. Deduped on the pid, since the two passes can
    // fail on it for different reasons.
    swept.unstoppable.sort();
    swept.unstoppable.dedup_by_key(|(pid, _)| *pid);
    swept
}

/// Sends `stop` to each pid, returning how many were signalled and collecting
/// the ones that could not be.
fn signal_all(pids: &[i32], stop: Stop, unstoppable: &mut Vec<(i32, String)>) -> usize {
    let mut signalled = 0usize;
    for pid in pids {
        match signal_process(*pid, stop) {
            Signalled::Sent => signalled = signalled.saturating_add(1),
            Signalled::Gone => {}
            Signalled::Failed(reason) => unstoppable.push((*pid, reason)),
        }
    }
    signalled
}

/// The outcome of signalling one process.
enum Signalled {
    /// The signal was delivered.
    Sent,
    /// The process was already gone: the "nothing to stop" answer, not a
    /// failure.
    Gone,
    /// The signal could not be delivered, so the process may still be running.
    Failed(String),
}

/// One process's children, and whether that list is all of them: a platform call
/// with a fixed-size buffer can report only as many as fit.
struct Children {
    pids: Vec<i32>,
    complete: bool,
}

/// One walk of the process tree.
struct Walk {
    pids: Vec<i32>,
    /// How many had been found when the walk stopped short, if it did.
    truncated_at: Option<usize>,
    /// Whether the walk refused to run at all because this process adopts
    /// orphans, and so cannot tell a command it started from a stranger's.
    declined: bool,
}

/// Every process descended from this one.
///
/// A command's own children are included: they are ordinary descendants, and
/// stopping only the command would leave them running, reparented away.
fn descendant_pids() -> Walk {
    let Ok(me) = i32::try_from(std::process::id()) else {
        return Walk {
            pids: Vec::new(),
            truncated_at: None,
            declined: false,
        };
    };
    // As pid 1 — a container entrypoint — every orphan in that container
    // reparents here and looks exactly like a descendant. A child subreaper
    // adopts orphans the same way: the attribute is not inherited by `fork` but
    // *is* preserved across `exec`, so a supervisor that sets it in the child
    // before exec'ing `task` gives it to the engine. Nothing in the walk can
    // tell a stranger's adopted orphan from a command this engine started, so
    // where either holds it declines to guess.
    if me == 1 || adopts_orphans() {
        return Walk {
            pids: Vec::new(),
            truncated_at: None,
            declined: true,
        };
    }

    let tree = ProcessTree::read();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut frontier = vec![me];
    // Bounded so a process table that reports a cycle cannot spin forever, and
    // so a runaway tree cannot be walked without end. The count is of nodes
    // expanded, checked after the pop, so one more than `MAX_WALK` is taken off
    // the frontier before the walk stops.
    let mut visited = 0usize;
    let mut truncated = false;
    while let Some(parent) = frontier.pop() {
        visited = visited.saturating_add(1);
        if visited > MAX_WALK || found.len() >= MAX_WALK {
            truncated = true;
            break;
        }
        let children = tree.children_of(parent);
        truncated |= !children.complete;
        for child in children.pids {
            // Checked per child, not only per parent: one node with a huge
            // fan-out would otherwise push the list far past the ceiling before
            // the outer check came round again.
            if found.len() >= MAX_WALK {
                truncated = true;
                break;
            }
            if child == me || !seen.insert(child) {
                continue;
            }
            // A zombie has already exited and its own children have reparented
            // away, so there is nothing below it and nothing to stop. Counting
            // it would overstate what the sweep did and buy a grace period
            // nobody is waiting for.
            if tree.is_zombie(child) {
                continue;
            }
            found.push(child);
            frontier.push(child);
        }
    }
    let truncated_at = truncated.then_some(found.len());
    Walk {
        pids: found,
        truncated_at,
        declined: false,
    }
}

/// A view of the process table for the duration of one walk.
struct ProcessTree {
    /// Children by parent pid, built once for a kernel that cannot answer per
    /// process. `None` when the per-process answer is available.
    #[cfg(target_os = "linux")]
    by_parent: Option<std::collections::HashMap<i32, Vec<i32>>>,
    /// The state byte of every process the scan above read, so a zombie check
    /// does not read the same `stat` a second time. Empty when there was no
    /// scan.
    #[cfg(target_os = "linux")]
    states: std::collections::HashMap<i32, u8>,
    /// Whether the scan above saw the whole of `/proc`. `false` leaves every
    /// node's child list unknown rather than empty, so the sweep says so
    /// instead of silently skipping a subtree.
    #[cfg(target_os = "linux")]
    scanned: bool,
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
impl ProcessTree {
    fn read() -> Self {
        if has_proc_children() {
            return Self {
                by_parent: None,
                states: std::collections::HashMap::new(),
                scanned: true,
            };
        }
        let (by_parent, states, scanned) = scan_proc_parents();
        Self {
            by_parent: Some(by_parent),
            states,
            scanned,
        }
    }

    fn children_of(&self, pid: i32) -> Children {
        match &self.by_parent {
            // One scan answers for the whole walk, so its completeness is the
            // scan's rather than this node's.
            Some(by_parent) => Children {
                pids: by_parent.get(&pid).cloned().unwrap_or_default(),
                complete: self.scanned,
            },
            None => children_from_proc(pid),
        }
    }

    /// Whether the process has exited and is only waiting to be reaped.
    fn is_zombie(&self, pid: i32) -> bool {
        match self.states.get(&pid) {
            Some(state) => *state == b'Z',
            None => read_stat(pid).is_some_and(|(state, _)| state == b'Z'),
        }
    }
}

/// Whether this process adopts orphans the way pid 1 does, so that a process
/// with no relation to any command could be walked as a descendant.
#[cfg(target_os = "linux")]
fn adopts_orphans() -> bool {
    const PR_GET_CHILD_SUBREAPER: i32 = 37;
    let mut set: i32 = 0;
    // SAFETY: for this option `prctl` writes one `int` through the pointer and
    // reads nothing else; the pointer is to a live local.
    unsafe { prctl(PR_GET_CHILD_SUBREAPER, &raw mut set) == 0 && set != 0 }
}

// Minimal FFI to `prctl(2)`, declared variadic as the C prototype is.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn prctl(option: core::ffi::c_int, ...) -> core::ffi::c_int;
}

/// No equivalent to ask for here, so the walk only refuses as pid 1.
#[cfg(not(target_os = "linux"))]
fn adopts_orphans() -> bool {
    false
}

/// Whether this kernel offers `/proc/<pid>/task/<tid>/children`
/// (`CONFIG_PROC_CHILDREN`).
///
/// Probed once: the answer cannot change under a running kernel, and the
/// fallback is expensive enough that it must not be reached for merely because a
/// process exited mid-walk.
#[cfg(target_os = "linux")]
fn has_proc_children() -> bool {
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT.get_or_init(|| {
        let Ok(threads) = std::fs::read_dir("/proc/self/task") else {
            return false;
        };
        threads
            .flatten()
            .any(|t| t.path().join("children").exists())
    })
}

/// Reads a process's children from `/proc`, where the kernel answers directly.
#[cfg(target_os = "linux")]
fn children_from_proc(pid: i32) -> Children {
    let mut pids = Vec::new();
    let mut complete = true;
    let threads = match std::fs::read_dir(format!("/proc/{pid}/task")) {
        Ok(threads) => threads,
        // Gone means nothing is left below it; anything else — `hidepid`, a
        // setuid child — means the subtree is unknown, not empty.
        Err(e) => {
            return Children {
                pids,
                complete: e.kind() == std::io::ErrorKind::NotFound,
            };
        }
    };
    for thread in threads.flatten() {
        let children = match std::fs::read_to_string(thread.path().join("children")) {
            Ok(children) => children,
            Err(e) => {
                // The thread going away mid-walk leaves nothing below it; being
                // unable to read it leaves its children unaccounted for.
                complete &= e.kind() == std::io::ErrorKind::NotFound;
                continue;
            }
        };
        pids.extend(
            children
                .split_ascii_whitespace()
                .filter_map(|f| f.parse::<i32>().ok()),
        );
    }
    Children { pids, complete }
}

/// Fallback for kernels without `/proc/<pid>/task/<tid>/children`: the parent
/// and state of every process, read once for the whole walk rather than once per
/// node.
#[cfg(target_os = "linux")]
fn scan_proc_parents() -> (
    std::collections::HashMap<i32, Vec<i32>>,
    std::collections::HashMap<i32, u8>,
    bool,
) {
    let mut by_parent: std::collections::HashMap<i32, Vec<i32>> = std::collections::HashMap::new();
    let mut states = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        // `/proc` itself unreadable: everything below this process is unknown.
        return (by_parent, states, false);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        let Some((state, ppid)) = read_stat(pid) else {
            // Almost always the process exiting mid-scan, which leaves nothing
            // below it. Anything else — an unreadable `stat` — would leave its
            // children unaccounted for, and the two are not worth telling apart
            // here: `/proc/<pid>/stat` is world-readable, so the second case
            // needs a hardened kernel that `has_proc_children` would have
            // steered away from this path anyway.
            continue;
        };
        by_parent.entry(ppid).or_default().push(pid);
        states.insert(pid, state);
    }
    (by_parent, states, true)
}

/// The state and parent pid out of `/proc/<pid>/stat`.
///
/// Read as bytes: `comm` is the process's own name, which need not be UTF-8, and
/// a `read_to_string` would skip such a process entirely — leaving it running.
#[cfg(target_os = "linux")]
fn read_stat(pid: i32) -> Option<(u8, i32)> {
    parse_stat(&std::fs::read(format!("/proc/{pid}/stat")).ok()?)
}

/// The state and parent pid out of one `/proc/<pid>/stat` line.
#[cfg(target_os = "linux")]
fn parse_stat(stat: &[u8]) -> Option<(u8, i32)> {
    // `comm` is parenthesised and may itself contain spaces and brackets, so the
    // fields after it start past the *last* ')'.
    let close = stat.iter().rposition(|b| *b == b')')?;
    let after_comm = stat.get(close.checked_add(1)?..)?;
    let mut fields = after_comm
        .split(u8::is_ascii_whitespace)
        .filter(|f| !f.is_empty());
    let state = *fields.next()?.first()?;
    let ppid = std::str::from_utf8(fields.next()?).ok()?.parse().ok()?;
    Some((state, ppid))
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
impl ProcessTree {
    fn read() -> Self {
        Self {}
    }

    /// No cheap way to ask here, so nothing is skipped: signalling an
    /// already-exited pid is a no-op beyond inflating the count and buying a
    /// grace period nobody is waiting for. Linux skips them.
    fn is_zombie(&self, _pid: i32) -> bool {
        false
    }

    /// Asks `libproc` for a process's children.
    fn children_of(&self, pid: i32) -> Children {
        /// Enough for any plausible number of concurrent commands.
        const CAPACITY: usize = 1024;
        /// `CAPACITY` pids, in bytes, as the call wants its buffer size.
        const CAPACITY_BYTES: i32 = 4096;
        // They have to agree: a `CAPACITY_BYTES` that overstates the buffer
        // invites a write past its end.
        const _: () = assert!(CAPACITY * size_of::<i32>() == CAPACITY_BYTES as usize);

        let mut buffer = vec![0i32; CAPACITY];
        // SAFETY: the buffer is `CAPACITY_BYTES` long and writable, which is what
        // the size argument promises. The call only writes pids into it.
        let written =
            unsafe { proc_listchildpids(pid, buffer.as_mut_ptr().cast(), CAPACITY_BYTES) };
        if written < 0 {
            const ESRCH: i32 = 3;
            // The process is gone, which is the ordinary answer, or it is one
            // this process may not ask about — and then a whole subtree is
            // missing from the walk rather than empty.
            let gone = std::io::Error::last_os_error().raw_os_error() == Some(ESRCH);
            return Children {
                pids: Vec::new(),
                complete: gone,
            };
        }
        // The return value is documented as a byte count in some places and a pid
        // count in others; the buffer was zeroed, so every pid actually written is
        // simply a non-zero entry, which is right under either reading.
        let pids: Vec<i32> = buffer.into_iter().filter(|pid| *pid > 0).collect();
        // A full buffer cannot be told from one that would have overflowed, so it
        // is reported as short rather than passed off as the whole list.
        let complete = pids.len() < CAPACITY;
        Children { pids, complete }
    }
}

// Minimal FFI to `proc_listchildpids(3)`, from libproc in libSystem — avoids
// pulling in a crate for one call, as `cache::lock` does for `kill`.
#[cfg(target_os = "macos")]
unsafe extern "C" {
    #[link_name = "proc_listchildpids"]
    fn proc_listchildpids(ppid: i32, buffer: *mut core::ffi::c_void, buffersize: i32) -> i32;
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// `PROCESSENTRY32`. Only the size, pid and parent pid are read; the rest is laid
/// out so that the size the API checks against is right.
#[cfg(windows)]
#[repr(C)]
struct ProcessEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [i8; 260],
}

/// `FILETIME`, read only as a creation timestamp to compare against another.
#[cfg(windows)]
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[cfg(windows)]
impl ProcessTree {
    fn read() -> Self {
        Self {}
    }

    /// No cheap way to ask here, so nothing is skipped: signalling an
    /// already-exited pid is a no-op beyond inflating the count and buying a
    /// grace period nobody is waiting for. Linux skips them.
    fn is_zombie(&self, _pid: i32) -> bool {
        false
    }

    /// Walks a process snapshot for entries whose parent is `pid`.
    ///
    /// One snapshot per node, where Linux's fallback builds one map for the
    /// whole walk: a command leaves a handful of processes behind, and hoisting
    /// it would only pay off near the `MAX_WALK` ceiling.
    ///
    /// A parent pid is *not* cleared when the parent exits, and Windows reuses
    /// pids freely, so an unrelated process can name a long-dead descendant of
    /// ours as its parent. Only a process created after its claimed parent is
    /// treated as its child; one whose creation time cannot be read is left
    /// alone rather than guessed at.
    fn children_of(&self, pid: i32) -> Children {
        const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
        const INVALID_HANDLE_VALUE: *mut core::ffi::c_void = usize::MAX as *mut core::ffi::c_void;

        let Ok(parent) = u32::try_from(pid) else {
            return Children {
                pids: Vec::new(),
                complete: true,
            };
        };
        let parent_created = match created_at(parent) {
            Ok(created) => created,
            // A pid that no longer exists has nothing below it; anything else
            // means a subtree is missing from the walk rather than empty.
            Err(err) => {
                return Children {
                    pids: Vec::new(),
                    complete: err == ERROR_INVALID_PARAMETER,
                };
            }
        };
        let mut pids = Vec::new();
        let mut complete = true;
        // SAFETY: the snapshot handle is checked before use and closed on every
        // path. `entry.dw_size` is set before the first call, as the API requires,
        // and only integer fields are read back.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
                return Children {
                    pids,
                    complete: false,
                };
            }
            let mut entry = std::mem::zeroed::<ProcessEntry32>();
            entry.dw_size = u32::try_from(std::mem::size_of::<ProcessEntry32>()).unwrap_or(0);
            let mut ok = Process32First(snapshot, &raw mut entry);
            while ok != 0 {
                if entry.th32_parent_process_id == parent
                    && let Ok(child) = i32::try_from(entry.th32_process_id)
                {
                    match created_at(entry.th32_process_id) {
                        // Created before its claimed parent: the pid was reused,
                        // and this is a stranger rather than a child.
                        Ok(created) if created >= parent_created => pids.push(child),
                        Ok(_) => {}
                        Err(ERROR_INVALID_PARAMETER) => {}
                        // Cannot be told apart from a child, so it is left alone
                        // and the list is reported as short.
                        Err(_) => complete = false,
                    }
                }
                ok = Process32Next(snapshot, &raw mut entry);
            }
            CloseHandle(snapshot);
        }
        Children { pids, complete }
    }
}

/// `ERROR_INVALID_PARAMETER`, which is what the process APIs answer for a pid
/// that no longer exists — as opposed to one this process may not touch.
#[cfg(windows)]
const ERROR_INVALID_PARAMETER: u32 = 87;

/// When a process was created, as a raw `FILETIME`. Only ever compared with
/// another of these, so the epoch does not matter. On failure, the OS error code
/// so the caller can tell a pid that is gone from one it may not open.
#[cfg(windows)]
fn created_at(pid: u32) -> Result<u64, u32> {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x0000_1000;

    // SAFETY: the handle is only used when non-null and is closed on every path;
    // the four timestamps are plain out-parameters this call fills in.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return Err(last_error());
        }
        let mut created = FileTime::default();
        let mut exited = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        let ok = GetProcessTimes(
            handle,
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        );
        let err = last_error();
        CloseHandle(handle);
        if ok == 0 {
            return Err(err);
        }
        Ok(u64::from(created.high_date_time).wrapping_shl(32) | u64::from(created.low_date_time))
    }
}

/// The last OS error code, or 0 if there is none.
#[cfg(windows)]
fn last_error() -> u32 {
    u32::try_from(std::io::Error::last_os_error().raw_os_error().unwrap_or(0)).unwrap_or(0)
}

#[cfg(windows)]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut core::ffi::c_void;
    fn Process32First(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry32) -> i32;
    fn Process32Next(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry32) -> i32;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> *mut core::ffi::c_void;
    fn GetProcessTimes(
        process: *mut core::ffi::c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
    fn TerminateProcess(process: *mut core::ffi::c_void, exit_code: u32) -> i32;
    fn CloseHandle(object: *mut core::ffi::c_void) -> i32;
}

// ---------------------------------------------------------------------------
// Signalling
// ---------------------------------------------------------------------------

/// Signals one process. Never a process group: the commands share this process's
/// group, so a group signal would take the engine down with them.
#[cfg(unix)]
fn signal_process(pid: i32, stop: Stop) -> Signalled {
    const ESRCH: i32 = 3;
    const SIGKILL: i32 = 9;
    const SIGTERM: i32 = 15;
    // `kill` reads 0 as "this whole process group" and -1 as "every process we
    // may signal", either of which would take the engine down with it. The walk
    // cannot produce those, and this keeps that property next to the one call
    // that would act on them.
    if pid <= 1 {
        return Signalled::Gone;
    }
    let sig = match stop {
        Stop::Terminate => SIGTERM,
        Stop::Kill => SIGKILL,
    };
    // SAFETY: `kill` only delivers a signal. `pid` is a descendant, so it is
    // never this process.
    if unsafe { libc_kill(pid, sig) } == 0 {
        return Signalled::Sent;
    }
    let err = std::io::Error::last_os_error();
    // ESRCH is the "nothing to stop" answer: the process finished on its own
    // between the walk and the signal. Anything else — EPERM on a setuid child —
    // leaves a process running that the sweep would otherwise claim to have
    // stopped, which must not be silent.
    if err.raw_os_error() == Some(ESRCH) {
        Signalled::Gone
    } else {
        Signalled::Failed(err.to_string())
    }
}

// Minimal FFI to `kill(2)`, as in `cache::lock`.
#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Terminates one process.
///
/// Windows has no gentler equivalent to reach for, so both kinds of [`Stop`] are
/// the same abrupt termination.
#[cfg(windows)]
fn signal_process(pid: i32, _stop: Stop) -> Signalled {
    const PROCESS_TERMINATE: u32 = 0x0001;
    let Ok(pid) = u32::try_from(pid) else {
        return Signalled::Gone;
    };
    // SAFETY: a handle is only used when non-null, and is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            let err = last_error();
            // A pid that no longer exists is the "nothing to stop" answer. Being
            // refused the handle is not: that process is still running, and the
            // sweep must not report it as stopped.
            return if err == ERROR_INVALID_PARAMETER {
                Signalled::Gone
            } else {
                Signalled::Failed(std::io::Error::from_raw_os_error(err.cast_signed()).to_string())
            };
        }
        let terminated = TerminateProcess(handle, 1) != 0;
        let err = std::io::Error::last_os_error();
        CloseHandle(handle);
        if terminated {
            Signalled::Sent
        } else {
            Signalled::Failed(err.to_string())
        }
    }
}

/// No way to walk the process tree on this platform, so nothing is found and
/// nothing is stopped.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl ProcessTree {
    fn read() -> Self {
        Self {}
    }

    /// No cheap way to ask here, so nothing is skipped: signalling an
    /// already-exited pid is a no-op beyond inflating the count and buying a
    /// grace period nobody is waiting for. Linux skips them.
    fn is_zombie(&self, _pid: i32) -> bool {
        false
    }

    fn children_of(&self, _pid: i32) -> Children {
        Children {
            pids: Vec::new(),
            complete: true,
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn signal_process(_pid: i32, _stop: Stop) -> Signalled {
    Signalled::Gone
}

#[cfg(test)]
mod tests {
    // Every test below is unix-only, so on Windows this import would be unused.
    #[cfg(unix)]
    use super::*;

    /// Kills a spawned process however the test ends, so a failed assertion does
    /// not leak a `sleep` for half a minute.
    #[cfg(unix)]
    struct Reaped(std::process::Child);

    #[cfg(unix)]
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    // The walk has to find a process we know is ours, and stop reporting it once
    // it has gone.
    #[cfg(unix)]
    #[test]
    fn finds_a_child_and_forgets_it_once_reaped() {
        let mut child = Reaped(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep"),
        );
        let pid = i32::try_from(child.0.id()).expect("pid fits");

        assert!(
            descendant_pids().pids.contains(&pid),
            "expected {pid} among {:?}",
            descendant_pids().pids
        );

        assert!(matches!(signal_process(pid, Stop::Kill), Signalled::Sent));
        child.0.wait().expect("reap sleep");
        assert!(!descendant_pids().pids.contains(&pid));
    }

    // A command's own children are what the walk exists for: stopping the
    // command alone would leave them running.
    #[cfg(unix)]
    #[test]
    fn finds_a_grandchild() {
        // `sh` stays alive as the child while `sleep` runs as the grandchild.
        let child = Reaped(
            std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 30 & wait")
                .spawn()
                .expect("spawn sh"),
        );
        let shell_pid = i32::try_from(child.0.id()).expect("pid fits");

        // The grandchild appears only once `sh` has got that far.
        let tree = ProcessTree::read();
        let mut grandchildren = Vec::new();
        for _ in 0..100 {
            grandchildren = tree.children_of(shell_pid).pids;
            if !grandchildren.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let walked = descendant_pids().pids;
        assert!(
            !grandchildren.is_empty(),
            "expected a child of {shell_pid}, walk saw {walked:?}"
        );
        assert!(
            walked.contains(&shell_pid),
            "expected the shell in {walked:?}"
        );
        for pid in &grandchildren {
            assert!(walked.contains(pid), "expected {pid} in {walked:?}");
        }

        // Only this test's own subtree is stopped: other tests in this binary
        // may have processes of their own running.
        for pid in grandchildren {
            let _ = signal_process(pid, Stop::Kill);
        }
        let _ = signal_process(shell_pid, Stop::Kill);
    }

    #[cfg(unix)]
    #[test]
    fn signalling_a_pid_that_is_gone_is_not_reported_as_stopped() {
        // A pid this process never had a child for: nothing to stop.
        assert!(matches!(
            signal_process(i32::MAX, Stop::Terminate),
            Signalled::Gone
        ));
        let mut unstoppable = Vec::new();
        assert_eq!(
            signal_all(&[i32::MAX], Stop::Terminate, &mut unstoppable),
            0
        );
        assert!(unstoppable.is_empty(), "a gone pid is not a failure");
    }

    // The `comm` field is the process's own name: it can hold spaces, brackets,
    // and bytes that are not UTF-8 at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn stat_is_parsed_past_a_comm_full_of_brackets() {
        assert_eq!(parse_stat(b"7 (we ) ird) S 3 3 0 0"), Some((b'S', 3)));
        assert_eq!(parse_stat(b"7 (sleep) Z 42 42 0 0"), Some((b'Z', 42)));
        // Not UTF-8, and still parsed rather than skipped.
        assert_eq!(parse_stat(b"7 (\xff\xfe) R 9 9 0 0"), Some((b'R', 9)));
        assert_eq!(parse_stat(b"truncated"), None);
        assert_eq!(parse_stat(b"7 (sleep)"), None);
        assert_eq!(parse_stat(b""), None);
    }

    // The test binary is an ordinary process: not pid 1, and nothing set the
    // subreaper attribute for it, so the walk is not refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_ordinary_process_does_not_adopt_orphans() {
        assert!(!adopts_orphans());
    }

    // This process is running, so it is not a zombie, and its own stat parses.
    #[cfg(target_os = "linux")]
    #[test]
    fn reads_this_process_state() {
        let me = i32::try_from(std::process::id()).expect("pid fits");
        let (state, _) = read_stat(me).expect("own stat");
        assert_ne!(state, b'Z');
        assert!(!ProcessTree::read().is_zombie(me));
    }
}

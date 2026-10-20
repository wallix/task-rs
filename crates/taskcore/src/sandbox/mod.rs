//! Restricting a task's commands to the files it declares.
//!
//! A task with `sandbox:` runs its commands — and the commands of the tasks
//! they call — on a thread restricted by a [`Policy`] built from its sources:
//! inside the root directory, reading anything else fails. It checks that
//! `sources` lists every input a build reads, which a missed entry otherwise
//! turns into a silently stale up-to-date check or cache hit.
//!
//! Two backends enforce the same rules: Landlock (`landlock`), cheap and
//! kernel-enforced, and a seccomp user-notification supervisor (`seccomp`), a
//! slower fallback that can report without denying accesses (`audit`).
//! `TASK_SANDBOX` picks one; see [`Mode`].

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
mod landlock;
mod policy;
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod seccomp;

/// The seccomp supervisor knows the syscall numbering of x86_64 and aarch64
/// only: elsewhere it never passes its probe.
#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
mod seccomp {
    use super::{Denials, Policy};
    use std::os::fd::OwnedFd;
    use std::sync::Arc;

    pub(crate) fn probe() -> Result<(), String> {
        Err("unsupported architecture".to_string())
    }

    pub(crate) fn install() -> std::io::Result<OwnedFd> {
        Err(std::io::Error::other("unsupported architecture"))
    }

    pub(crate) fn supervise(_: OwnedFd, _: Vec<Arc<Policy>>, _: bool, _: Denials) {}
}

/// Checks that the seccomp supervisor works here; blocks the first time.
pub fn supervisor_available() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        seccomp::probe()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("seccomp needs Linux".to_string())
    }
}

pub(crate) use policy::{CreatedDir, remove_created};
pub use policy::{Policy, PolicyInput, remove_stale_tmp, remove_tmp};

/// What `TASK_SANDBOX` asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Landlock when the kernel has it, else the seccomp supervisor with a
    /// warning, else unrestricted with a warning.
    #[default]
    Auto,
    /// Landlock, or fail.
    Landlock,
    /// The seccomp supervisor, enforcing.
    Seccomp,
    /// The seccomp supervisor, reporting what would be denied without denying it.
    Audit,
    /// Ignore `sandbox:`.
    Off,
}

impl Mode {
    /// Parses a `TASK_SANDBOX` value; empty means [`Mode::Auto`].
    pub fn parse(value: &str) -> Result<Self, UnknownMode> {
        match value {
            "" | "auto" => Ok(Self::Auto),
            "landlock" => Ok(Self::Landlock),
            "seccomp" => Ok(Self::Seccomp),
            "audit" => Ok(Self::Audit),
            "off" | "0" => Ok(Self::Off),
            other => Err(UnknownMode(other.to_string())),
        }
    }
}

/// A `TASK_SANDBOX` value [`Mode::parse`] does not know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownMode(pub String);

impl std::fmt::Display for UnknownMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TASK_SANDBOX={:?}: expected auto, landlock, seccomp, audit or off",
            self.0
        )
    }
}

impl std::error::Error for UnknownMode {}

/// How commands run after resolving [`Mode`] against the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Landlock, at this ABI version.
    Landlock(i64),
    /// The seccomp supervisor; `enforce: false` only reports.
    Seccomp { enforce: bool },
    /// Unrestricted.
    None,
}

impl Backend {
    /// Resolves `mode` against the kernel. Returns a warning when [`Mode::Auto`]
    /// falls back, or an error when the requested mode is unavailable.
    pub fn resolve(mode: Mode) -> Result<(Self, Option<String>), String> {
        #[cfg(target_os = "linux")]
        {
            if mode == Mode::Off {
                return Ok((Self::None, None));
            }
            let abi = landlock::abi().map_err(|e| e.to_string());
            Self::pick(mode, abi, seccomp::probe)
        }
        #[cfg(not(target_os = "linux"))]
        match mode {
            Mode::Off => Ok((Self::None, None)),
            Mode::Auto => Ok((
                Self::None,
                Some("sandboxing needs Linux; commands run unrestricted".to_string()),
            )),
            Mode::Landlock => Err("TASK_SANDBOX=landlock needs Linux".to_string()),
            Mode::Seccomp => Err("TASK_SANDBOX=seccomp needs Linux".to_string()),
            Mode::Audit => Err("TASK_SANDBOX=audit needs Linux".to_string()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Backend {
    /// [`Backend::resolve`] given the kernel's Landlock ABI (or why it has
    /// none) and a probe of the seccomp supervisor, run only when needed.
    fn pick(
        mode: Mode,
        landlock: Result<i64, String>,
        seccomp: impl FnOnce() -> Result<(), String>,
    ) -> Result<(Self, Option<String>), String> {
        let landlock = match landlock {
            Ok(v) if v >= landlock::MIN_ABI => Ok(v),
            Ok(v) => Err(format!("Landlock ABI {v} is below {}", landlock::MIN_ABI)),
            Err(e) => Err(format!("Landlock is unavailable ({e})")),
        };
        match (mode, landlock) {
            (Mode::Off, _) => Ok((Self::None, None)),
            (Mode::Auto | Mode::Landlock, Ok(v)) => Ok((Self::Landlock(v), None)),
            (Mode::Landlock, Err(why)) => Err(format!("TASK_SANDBOX=landlock: {why}")),
            (Mode::Auto, Err(why)) => match seccomp() {
                Ok(()) => Ok((
                    Self::Seccomp { enforce: true },
                    Some(format!(
                        "{why}; sandboxing with the slower seccomp supervisor"
                    )),
                )),
                Err(e) => Ok((
                    Self::None,
                    Some(format!(
                        "{why}, and seccomp is unavailable ({e}); commands run unrestricted"
                    )),
                )),
            },
            (Mode::Seccomp | Mode::Audit, _) => {
                let enforce = mode == Mode::Seccomp;
                let name = if enforce { "seccomp" } else { "audit" };
                seccomp()
                    .map_err(|e| format!("TASK_SANDBOX={name}: seccomp is unavailable ({e})"))?;
                Ok((Self::Seccomp { enforce }, None))
            }
        }
    }
}

/// An access the sandbox denied (or, auditing, would have denied).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Denial {
    /// An access a policy does not allow.
    Refused {
        /// The task whose policy refused it.
        task: String,
        /// What was attempted: `read`, `write`, `create`, `list`, ...
        access: &'static str,
        /// The path, or the directory an entry was created or removed in.
        path: PathBuf,
    },
    /// Syscalls of another architecture (or x32), whose paths the seccomp
    /// supervisor does not check: failed with `EACCES` when enforcing, let
    /// through when auditing. Noted once per architecture, and not counted
    /// as a denial.
    Foreign {
        /// Which: `x32 syscalls`, `syscalls of architecture 0x40000003`.
        syscalls: String,
    },
    /// A syscall whose paths the seccomp supervisor could not resolve:
    /// denied when enforcing, let through unchecked when auditing.
    Unchecked {
        /// The syscall's name.
        syscall: &'static str,
        /// Why it could not be checked.
        why: String,
    },
}

/// What a supervisor saw.
#[derive(Debug, Default)]
pub struct Seen {
    /// The denials to report, since the engine last took them: each distinct
    /// one once per supervisor.
    pub denials: Vec<Denial>,
    /// How many accesses were denied, reported or not: a command was denied
    /// something when it grew while the command ran.
    pub count: u64,
}

/// Where a supervisor records its denials, for the engine to report.
pub type Denials = Arc<Mutex<Seen>>;

/// A command to run on a sandbox's worker thread: it builds the future there.
pub(crate) type Job =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send>;

/// The policies of the sandboxed tasks a command runs within, outermost first,
/// and what the commands under them share: the thread they run on, restricted
/// once on first use, and what the seccomp supervisor saw.
#[derive(Debug)]
pub struct Layers {
    /// The policies; a command may do what all of them allow.
    pub policies: Vec<Arc<Policy>>,
    /// The denials the seccomp supervisor saw. Landlock reports none: its
    /// denials are the `EACCES` errors the command prints.
    denials: Denials,
    /// Set when the seccomp backend found this process already under a
    /// supervisor — a sandboxed command of an enclosing `task` started it — so
    /// that only the enclosing layers apply.
    unenforced: Arc<AtomicBool>,
    /// The worker thread, or why it could not be restricted.
    worker: tokio::sync::OnceCell<Result<Worker, String>>,
    /// The same policies under the seccomp supervisor, for the `if` of
    /// commands run under Landlock (see [`Sandbox::for_condition`]).
    condition: std::sync::OnceLock<Arc<Layers>>,
}

impl Layers {
    /// The layers of `outer` (if any) with `policy` inside them.
    pub fn push(outer: Option<&Self>, policy: Arc<Policy>) -> Arc<Self> {
        let mut policies = outer.map(|o| o.policies.clone()).unwrap_or_default();
        policies.push(policy);
        Self::new(policies)
    }

    /// Layers made of `policies`, outermost first.
    pub fn new(policies: Vec<Arc<Policy>>) -> Arc<Self> {
        Arc::new(Self {
            policies,
            denials: Arc::new(Mutex::new(Seen::default())),
            unenforced: Arc::new(AtomicBool::new(false)),
            worker: tokio::sync::OnceCell::new(),
            condition: std::sync::OnceLock::new(),
        })
    }
}

/// A thread restricted by a sandbox, running its commands as tasks of a
/// runtime of its own: every thread and process a command starts from it
/// inherits the restriction, and the engine's threads keep their access.
/// Dropping it lets the thread finish, cancelling the commands still running.
#[derive(Debug)]
struct Worker {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl Worker {
    /// Starts the thread and waits for it to be restricted.
    fn start(backend: Backend, layers: &Layers) -> Result<Self, String> {
        let restrict = restriction(backend, layers).map_err(|e| e.to_string())?;
        let (jobs, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        spawn_thread(move || {
            let rt = restrict().map_err(|e| e.to_string()).and_then(|()| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())
            });
            // A failed send means `start` is gone, and nobody is left to tell.
            let rt = match rt {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                while let Some(job) = rx.recv().await {
                    tokio::task::spawn_local(job());
                }
            });
        })
        .map_err(|e| e.to_string())?;
        ready
            .recv()
            .unwrap_or_else(|_| Err("the sandbox thread died".to_string()))?;
        Ok(Self { jobs })
    }
}

/// What restricts the worker thread of `layers`, run on that thread; anything
/// it needs set up beforehand, from an unrestricted thread, is set up here.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn restriction(
    backend: Backend,
    layers: &Layers,
) -> io::Result<Box<dyn FnOnce() -> io::Result<()> + Send>> {
    match backend {
        Backend::None => Ok(Box::new(|| Ok(()))),
        #[cfg(target_os = "linux")]
        Backend::Landlock(abi) => {
            let policies = layers.policies.clone();
            Ok(Box::new(move || landlock::restrict(&policies, abi)))
        }
        #[cfg(target_os = "linux")]
        Backend::Seccomp { enforce } => {
            // Under Landlock, only a command's `if` gets here: it must not
            // find out at its first syscall that the supervisor cannot work.
            seccomp::probe()
                .map_err(|e| io::Error::other(format!("seccomp is unavailable: {e}")))?;
            // The supervisor comes first, from this unfiltered thread.
            let (tx, rx) = std::sync::mpsc::channel::<std::os::fd::OwnedFd>();
            let policies = layers.policies.clone();
            let denials = Arc::clone(&layers.denials);
            spawn_thread(move || {
                if let Ok(listener) = rx.recv() {
                    seccomp::supervise(listener, policies, enforce, denials);
                }
            })?;
            let unenforced = Arc::clone(&layers.unenforced);
            Ok(Box::new(move || match seccomp::install() {
                Ok(listener) => {
                    // The supervisor thread only ends once `tx` is dropped, so
                    // it is there to receive.
                    let _ = tx.send(listener);
                    Ok(())
                }
                // A command of an enclosing sandboxed task started this
                // process: its supervisor checks this thread, against its own
                // layers only.
                Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                    unenforced.store(true, Ordering::Relaxed);
                    Ok(())
                }
                Err(e) => Err(io::Error::new(
                    e.kind(),
                    format!("seccomp is unavailable: {e}"),
                )),
            }))
        }
        #[cfg(not(target_os = "linux"))]
        _ => Ok(Box::new(|| Ok(()))),
    }
}

/// What a command's `if` runs under (see [`Sandbox::for_condition`]).
#[derive(Debug)]
pub enum Condition {
    /// This sandbox.
    Sandboxed(Sandbox),
    /// No sandbox: the command's own runs unrestricted too.
    Unsandboxed,
    /// No sandbox, although the command's own runs under Landlock: the
    /// seccomp supervisor is unavailable, for this reason.
    Unchecked(String),
}

/// What a sandboxed command runs under.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// How to enforce it.
    pub backend: Backend,
    /// What it enforces.
    pub layers: Arc<Layers>,
    /// Whether this is the sandbox of the `if` of a command that runs under
    /// Landlock (see [`Sandbox::for_condition`]).
    pub landlock_condition: bool,
}

impl Sandbox {
    /// A sandbox for `layers`.
    pub fn new(backend: Backend, layers: Arc<Layers>) -> Self {
        Self {
            backend,
            layers,
            landlock_condition: false,
        }
    }

    /// What a command's `if` runs under: the same policies with seccomp,
    /// even under Landlock. Landlock would make a denied condition false
    /// and skip the command as if it did not apply; the supervisor reports the
    /// denial so it fails the task instead. `supervisor` is
    /// [`supervisor_available`], consulted only under Landlock: without it
    /// the condition runs unsandboxed, as it would without the sandbox.
    pub fn for_condition(&self, supervisor: impl FnOnce() -> Result<(), String>) -> Condition {
        match self.backend {
            Backend::Landlock(_) => match supervisor() {
                Ok(()) => {
                    let layers = self
                        .layers
                        .condition
                        .get_or_init(|| Layers::new(self.layers.policies.clone()));
                    Condition::Sandboxed(Self {
                        landlock_condition: true,
                        ..Self::new(Backend::Seccomp { enforce: true }, Arc::clone(layers))
                    })
                }
                Err(why) => Condition::Unchecked(why),
            },
            Backend::Seccomp { .. } => Condition::Sandboxed(self.clone()),
            Backend::None => Condition::Unsandboxed,
        }
    }

    /// The `TMPDIR` the command needs, if a layer has one.
    pub fn tmpdir(&self) -> Option<&std::path::Path> {
        self.layers
            .policies
            .iter()
            .rev()
            .find_map(|l| l.tmpdir.as_deref())
    }

    /// Takes recorded denials for reporting. Concurrent commands under the
    /// same layers share them, so one may report another's.
    pub fn take_denials(&self) -> Vec<Denial> {
        let mut seen = self
            .layers
            .denials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut seen.denials)
    }

    /// How many accesses were denied under these layers so far.
    pub fn denial_count(&self) -> u64 {
        self.layers
            .denials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .count
    }

    /// Reports whether an enclosing seccomp supervisor leaves these layers
    /// unenforced.
    pub fn unenforced(&self) -> bool {
        self.layers.unenforced.load(Ordering::Relaxed)
    }

    /// Runs `job` on the layers' worker thread, starting it on first use. The
    /// start blocks until the thread is restricted, which builds the Landlock
    /// rulesets: it runs off the engine's thread.
    pub(crate) async fn run(&self, job: Job) -> io::Result<()> {
        let worker = self
            .layers
            .worker
            .get_or_init(|| {
                let (backend, layers) = (self.backend, Arc::clone(&self.layers));
                async move {
                    tokio::task::spawn_blocking(move || Worker::start(backend, &layers))
                        .await
                        .unwrap_or_else(|e| Err(format!("starting the sandbox thread: {e}")))
                }
            })
            .await;
        match worker {
            Ok(worker) => worker
                .jobs
                .send(job)
                .map_err(|_| io::Error::other("the sandbox thread died")),
            Err(e) => Err(io::Error::other(e.clone())),
        }
    }
}

fn spawn_thread(f: impl FnOnce() + Send + 'static) -> io::Result<()> {
    std::thread::Builder::new()
        .name("task-sandbox".to_string())
        .spawn(f)
        .map(drop)
}

/// Sets `no_new_privs` on the calling thread, which both backends require of an
/// unprivileged caller.
#[cfg(target_os = "linux")]
fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: plain prctl with integer arguments.
    let ret = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_condition_runs_unchecked_without_the_supervisor_under_landlock() {
        let sandbox = |backend| Sandbox::new(backend, Layers::new(Vec::new()));
        let unavailable = || Err("EPERM".to_string());
        let unreached = || -> Result<(), String> { panic!("probed needlessly") };
        assert!(matches!(
            sandbox(Backend::Landlock(4)).for_condition(unavailable),
            Condition::Unchecked(why) if why == "EPERM"
        ));
        let Condition::Sandboxed(s) = sandbox(Backend::Landlock(4)).for_condition(|| Ok(())) else {
            panic!("expected the supervisor");
        };
        assert_eq!(s.backend, Backend::Seccomp { enforce: true });
        assert!(s.landlock_condition);
        for enforce in [true, false] {
            let Condition::Sandboxed(s) =
                sandbox(Backend::Seccomp { enforce }).for_condition(unreached)
            else {
                panic!("expected the same sandbox");
            };
            assert_eq!(s.backend, Backend::Seccomp { enforce });
        }
        assert!(matches!(
            sandbox(Backend::None).for_condition(unreached),
            Condition::Unsandboxed
        ));
    }

    #[test]
    fn parses_modes() {
        assert_eq!(Mode::parse(""), Ok(Mode::Auto));
        assert_eq!(Mode::parse("landlock"), Ok(Mode::Landlock));
        assert_eq!(Mode::parse("seccomp"), Ok(Mode::Seccomp));
        assert_eq!(Mode::parse("audit"), Ok(Mode::Audit));
        assert_eq!(Mode::parse("0"), Ok(Mode::Off));
        assert_eq!(
            Mode::parse("on").unwrap_err().to_string(),
            "TASK_SANDBOX=\"on\": expected auto, landlock, seccomp, audit or off"
        );
    }

    #[test]
    fn off_never_restricts() {
        assert_eq!(Backend::resolve(Mode::Off), Ok((Backend::None, None)));
    }

    #[cfg(target_os = "linux")]
    mod pick {
        use super::*;

        fn no_landlock() -> Result<i64, String> {
            Err("ENOSYS".to_string())
        }

        fn seccomp_ok() -> Result<(), String> {
            Ok(())
        }

        fn seccomp_fails() -> Result<(), String> {
            Err("EINVAL".to_string())
        }

        fn unreached() -> Result<(), String> {
            panic!("seccomp probed needlessly")
        }

        #[test]
        fn landlock_when_the_kernel_has_it() {
            for mode in [Mode::Auto, Mode::Landlock] {
                assert_eq!(
                    Backend::pick(mode, Ok(4), unreached),
                    Ok((Backend::Landlock(4), None))
                );
            }
        }

        #[test]
        fn landlock_mode_fails_without_it() {
            let err = Backend::pick(Mode::Landlock, no_landlock(), unreached).unwrap_err();
            assert!(err.contains("Landlock is unavailable"), "{err}");
            let err = Backend::pick(Mode::Landlock, Ok(1), unreached).unwrap_err();
            assert!(err.contains("is below"), "{err}");
        }

        #[test]
        fn auto_falls_back_to_seccomp_with_a_warning() {
            let (backend, warning) = Backend::pick(Mode::Auto, Ok(1), seccomp_ok).unwrap();
            assert_eq!(backend, Backend::Seccomp { enforce: true });
            assert!(warning.unwrap().contains("seccomp supervisor"));
        }

        #[test]
        fn auto_with_neither_runs_unrestricted_with_a_warning() {
            let (backend, warning) =
                Backend::pick(Mode::Auto, no_landlock(), seccomp_fails).unwrap();
            assert_eq!(backend, Backend::None);
            assert!(warning.unwrap().contains("unrestricted"));
        }

        #[test]
        fn seccomp_modes_need_a_working_supervisor() {
            assert_eq!(
                Backend::pick(Mode::Seccomp, Ok(4), seccomp_ok),
                Ok((Backend::Seccomp { enforce: true }, None))
            );
            assert_eq!(
                Backend::pick(Mode::Audit, no_landlock(), seccomp_ok),
                Ok((Backend::Seccomp { enforce: false }, None))
            );
            let err = Backend::pick(Mode::Seccomp, Ok(4), seccomp_fails).unwrap_err();
            assert!(
                err.contains("TASK_SANDBOX=seccomp: seccomp is unavailable"),
                "{err}"
            );
        }
    }
}

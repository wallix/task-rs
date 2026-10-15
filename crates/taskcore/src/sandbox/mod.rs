//! Restricting a task's commands to the files it declares.
//!
//! A task with `sandbox:` runs its commands — and the commands of the tasks
//! they call — on a thread restricted by a [`Policy`] built from its sources:
//! inside the root directory, reading anything else fails. It checks that
//! `sources` lists every input a build reads, which a missed entry otherwise
//! turns into a silently stale up-to-date check or cache hit.
//!
//! Landlock (`landlock`) enforces the rules, in the kernel. `TASK_SANDBOX`
//! picks whether; see [`Mode`].

use std::io;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod landlock;
mod policy;

pub use policy::{Policy, PolicyInput, remove_stale_tmp, remove_tmp};

/// What `TASK_SANDBOX` asks for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Landlock when the kernel has it, else unrestricted with a warning.
    #[default]
    Auto,
    /// Landlock, or fail.
    Landlock,
    /// Ignore `sandbox:`.
    Off,
}

impl Mode {
    /// Parses a `TASK_SANDBOX` value; empty means [`Mode::Auto`].
    pub fn parse(value: &str) -> Result<Self, UnknownMode> {
        match value {
            "" | "auto" => Ok(Self::Auto),
            "landlock" => Ok(Self::Landlock),
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
            "TASK_SANDBOX={:?}: expected auto, landlock or off",
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
            Self::pick(mode, abi)
        }
        #[cfg(not(target_os = "linux"))]
        match mode {
            Mode::Off => Ok((Self::None, None)),
            Mode::Auto => Ok((
                Self::None,
                Some("sandboxing needs Linux; commands run unrestricted".to_string()),
            )),
            Mode::Landlock => Err("TASK_SANDBOX=landlock needs Linux".to_string()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Backend {
    /// [`Backend::resolve`] given the kernel's Landlock ABI (or why it has
    /// none).
    fn pick(mode: Mode, landlock: Result<i64, String>) -> Result<(Self, Option<String>), String> {
        let landlock = match landlock {
            Ok(v) if v >= landlock::MIN_ABI => Ok(v),
            Ok(v) => Err(format!("Landlock ABI {v} is below {}", landlock::MIN_ABI)),
            Err(e) => Err(format!("Landlock is unavailable ({e})")),
        };
        match (mode, landlock) {
            (Mode::Off, _) => Ok((Self::None, None)),
            (Mode::Auto | Mode::Landlock, Ok(v)) => Ok((Self::Landlock(v), None)),
            (Mode::Landlock, Err(why)) => Err(format!("TASK_SANDBOX=landlock: {why}")),
            (Mode::Auto, Err(why)) => Ok((
                Self::None,
                Some(format!("{why}; commands run unrestricted")),
            )),
        }
    }
}

/// A command to run on a sandbox's worker thread: it builds the future there.
pub(crate) type Job =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send>;

/// The policies of the sandboxed tasks a command runs within, outermost first,
/// and the thread the commands under them run on, restricted once on first
/// use.
#[derive(Debug)]
pub struct Layers {
    /// The policies; a command may do what all of them allow.
    pub policies: Vec<Arc<Policy>>,
    /// The worker thread, or why it could not be restricted.
    worker: tokio::sync::OnceCell<Result<Worker, String>>,
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
            worker: tokio::sync::OnceCell::new(),
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
        #[cfg(not(target_os = "linux"))]
        _ => Ok(Box::new(|| Ok(()))),
    }
}

/// What a sandboxed command runs under.
#[derive(Clone, Debug)]
pub struct Sandbox {
    /// How to enforce it.
    pub backend: Backend,
    /// What it enforces.
    pub layers: Arc<Layers>,
}

impl Sandbox {
    /// A sandbox for `layers`.
    pub fn new(backend: Backend, layers: Arc<Layers>) -> Self {
        Self { backend, layers }
    }

    /// The `TMPDIR` the command needs, if a layer has one.
    pub fn tmpdir(&self) -> Option<&std::path::Path> {
        self.layers
            .policies
            .iter()
            .rev()
            .find_map(|l| l.tmpdir.as_deref())
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

/// Sets `no_new_privs` on the calling thread, which Landlock requires of an
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
    fn parses_modes() {
        assert_eq!(Mode::parse(""), Ok(Mode::Auto));
        assert_eq!(Mode::parse("landlock"), Ok(Mode::Landlock));
        assert!(Mode::parse("seccomp").is_err());
        assert_eq!(Mode::parse("0"), Ok(Mode::Off));
        assert_eq!(
            Mode::parse("on").unwrap_err().to_string(),
            "TASK_SANDBOX=\"on\": expected auto, landlock or off"
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

        #[test]
        fn landlock_when_the_kernel_has_it() {
            for mode in [Mode::Auto, Mode::Landlock] {
                assert_eq!(Backend::pick(mode, Ok(4)), Ok((Backend::Landlock(4), None)));
            }
        }

        #[test]
        fn landlock_mode_fails_without_it() {
            let err = Backend::pick(Mode::Landlock, no_landlock()).unwrap_err();
            assert!(err.contains("Landlock is unavailable"), "{err}");
            let err = Backend::pick(Mode::Landlock, Ok(1)).unwrap_err();
            assert!(err.contains("is below"), "{err}");
        }

        #[test]
        fn auto_without_landlock_runs_unrestricted_with_a_warning() {
            for abi in [no_landlock(), Ok(1)] {
                let (backend, warning) = Backend::pick(Mode::Auto, abi).unwrap();
                assert_eq!(backend, Backend::None);
                assert!(warning.unwrap().contains("unrestricted"));
            }
        }
    }
}

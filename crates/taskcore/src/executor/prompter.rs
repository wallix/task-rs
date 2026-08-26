//! Interactive prompting, abstracted so the TUI stays out of the engine.
//!
//! The Go implementation prompts for missing required variables and yes/no
//! confirmations using a bubbletea-based `input` package. Porting a terminal UI
//! into the engine would drag in unrelated dependencies, so the engine instead
//! holds an optional [`Prompter`]. The CLI supplies a concrete implementation;
//! when none is set the engine behaves as a non-interactive `--yes=false`
//! session: confirmations are declined and variable prompts are unavailable.

/// An error raised by a prompt.
#[derive(Debug)]
pub enum PromptError {
    /// The user cancelled the prompt (e.g. Ctrl-C).
    Cancelled,
    /// The prompt could not run (no terminal, I/O failure, …).
    Unavailable(String),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => write!(f, "prompt cancelled"),
            Self::Unavailable(msg) => write!(f, "prompt unavailable: {msg}"),
        }
    }
}

impl std::error::Error for PromptError {}

/// A source of interactive answers. Implementations drive whatever UI is
/// appropriate (a real TUI in the CLI, a scripted stub in tests).
///
/// The methods block: they are called on a blocking thread rather than on the
/// runtime, so that waiting for an answer does not stop the engine from being
/// polled — an interrupt arriving at a prompt still gets handled. That is why an
/// implementation has to be `Send + Sync`.
///
/// Only prompts that go through a `Prompter` get that. An embedder that installs
/// none falls back to the logger's own yes/no read, which still runs on the
/// runtime thread; the `task` binary always installs one.
pub trait Prompter: Send + Sync {
    /// Asks the user to confirm `message`, returning whether they accepted.
    fn confirm(&self, message: &str) -> Result<bool, PromptError>;

    /// Asks the user for the value of variable `name`. When `enum_values` is
    /// non-empty the answer is constrained to one of those choices.
    fn prompt(&self, name: &str, enum_values: &[String]) -> Result<String, PromptError>;
}

/// A prompt to run on the terminal thread.
type Ask = Box<dyn FnOnce() + Send>;

/// The queue feeding the one thread that talks to the terminal, started with the
/// first prompt.
fn terminal_thread() -> &'static std::sync::mpsc::Sender<Ask> {
    // No mutex: `Sender` is `Sync`, so the queue can be shared as it is.
    static QUEUE: std::sync::OnceLock<std::sync::mpsc::Sender<Ask>> = std::sync::OnceLock::new();
    QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Ask>();
        // Detached: it spends its life blocked on a terminal that may never
        // answer, and there is nothing to wait for it at exit.
        //
        // A thread that cannot be spawned leaves the queue with no reader, and
        // every prompt then fails with `unreachable_terminal` rather than
        // bringing the run down here.
        let spawned = std::thread::Builder::new()
            .name("task-prompt".to_string())
            .spawn(move || {
                while let Ok(ask) = rx.recv() {
                    ask();
                }
            });
        drop(spawned);
        tx
    })
}

/// Runs a blocking prompt on the terminal thread, so that waiting for the user
/// does not stop the engine's own tasks — the interrupt watcher above all — from
/// being polled.
///
/// One thread serves every prompt, which the read being uncancellable makes
/// necessary rather than merely tidy. A prompt abandoned part-way — a failing
/// sibling tearing the run down — keeps reading until the terminal gives it a
/// line, so a thread per prompt would leave one behind on every `--watch`
/// iteration, each parked on a read nothing will answer. Sharing one thread also
/// keeps two prompts from asking their questions over each other: the queue
/// hands them out in turn.
pub(crate) async fn asked_off_thread<T, F>(ask: F) -> Result<T, super::ExecutorError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let job: Ask = Box::new(move || {
        // A prompt abandoned before its turn came is not asked at all: the read
        // cannot be cancelled once started, but a queued one still can be, and
        // asking would print a question nobody is waiting for and swallow the
        // line meant for the next prompt.
        if tx.is_closed() {
            return;
        }
        // Caught rather than left to kill the thread every later prompt needs;
        // re-raised below, since a panicking prompter is a bug in it and not a
        // failed prompt.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(ask));
        // Discarded deliberately: a send only fails when the prompt was
        // abandoned while the read was in flight, and there is nobody left to
        // tell.
        let _ = tx.send(outcome);
    });
    terminal_thread()
        .send(job)
        .map_err(|_| unreachable_terminal())?;
    match rx.await {
        Ok(Ok(answer)) => Ok(answer),
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => Err(unreachable_terminal()),
    }
}

/// The prompt thread cannot answer: it could not be spawned, or a panic escaped
/// the catch above. The queue itself never closes — its sender is a static that
/// outlives the process — so those are the only two ways.
fn unreachable_terminal() -> super::ExecutorError {
    super::ExecutorError::Io("prompt failed: the terminal is no longer being read".to_string())
}

//! Interrupt handling.
//!
//! The engine intercepts SIGINT/SIGTERM so that a running command is given a
//! chance to clean up rather than the process being killed instantly. The first
//! two signals log a notice; a third forces an immediate exit. Ports Go
//! `InterceptInterruptSignals`, using [`tokio::signal`] in place of the Go
//! `os/signal` channel.

use std::rc::Rc;

use crate::logger::Color;

use super::Executor;

/// The number of interrupt signals tolerated before forcing shutdown.
pub const MAX_INTERRUPT_SIGNALS: usize = 3;

impl Executor {
    /// Spawns a task that intercepts interrupt signals. The first
    /// [`MAX_INTERRUPT_SIGNALS`] − 1 signals log a notice; the final one exits
    /// the process. Returns immediately; the watcher runs until the process
    /// ends. Ports Go `InterceptInterruptSignals`.
    ///
    /// Must be called from within a Tokio runtime context (a `LocalSet`, since
    /// the engine is single-threaded).
    pub fn intercept_interrupt_signals(self: &Rc<Self>) {
        let this = Rc::clone(self);
        tokio::task::spawn_local(async move {
            for i in 0..MAX_INTERRUPT_SIGNALS {
                if next_interrupt().await.is_err() {
                    return;
                }
                if i.saturating_add(1) >= MAX_INTERRUPT_SIGNALS {
                    this.logger().borrow_mut().errf(
                        Color::Red,
                        "task: Signal received for the third time. Forcing shutdown\n",
                    );
                    std::process::exit(1);
                }
                this.logger()
                    .borrow_mut()
                    .outf(Color::Yellow, "task: Signal received\n");
            }
        });
    }
}

/// Awaits the next SIGINT or SIGTERM. Returns `Err` if signal handlers cannot be
/// installed.
async fn next_interrupt() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt())?;
        let mut sigterm = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = sigint.recv() => Ok(()),
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

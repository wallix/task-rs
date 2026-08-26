//! Interrupt handling.
//!
//! The engine intercepts SIGINT/SIGTERM so that a running command is given a
//! chance to clean up rather than the process being killed instantly. The first
//! two *interrupts* log a notice and a third forces an immediate exit; a
//! `SIGTERM` forces one straight away. Ports Go
//! `InterceptInterruptSignals`, using [`tokio::signal`] in place of the Go
//! `os/signal` channel.
//!
//! The first *interrupt* is only reported. A terminal delivers it to the whole
//! foreground process group, which the commands are in, so they have already had
//! it — and a signal on top of that would cut short a handler still cleaning up.
//! From the second on, the same signal is passed to the commands. The forced
//! exit stops whatever is still running first, since nothing else will once the
//! process is gone.
//!
//! A `SIGTERM` is not treated that way, which is where this departs from Task
//! v3. That reasoning is about a terminal, and a `SIGTERM` does not come from
//! one: it is sent by pid, by a supervisor, so nothing reached the commands and
//! there is no handler of theirs to cut short. Reporting it and running on means
//! a supervisor's `SIGTERM`-then-`SIGKILL` gets no cleanup at all — the commands
//! keep running and the run keeps starting more. So the first `SIGTERM` acts at
//! once: the commands are stopped and the process exits.

use std::rc::Rc;

use crate::logger::Color;

use super::Executor;

/// The number of interrupt signals tolerated before forcing shutdown.
pub const MAX_INTERRUPT_SIGNALS: usize = 3;

/// The name Go's `os.Signal` prints for a signal, so the two lines this module
/// logs read the same as Task v3's.
fn signal_name(stop: crate::reap::Stop) -> &'static str {
    match stop {
        crate::reap::Stop::Interrupt => "interrupt",
        crate::reap::Stop::Terminate => "terminated",
        // Never relayed from the handler: it only ever sees the two above.
        crate::reap::Stop::Kill => "killed",
    }
}

impl Executor {
    /// Spawns a task that intercepts interrupt signals. The first
    /// [`MAX_INTERRUPT_SIGNALS`] − 1 *interrupts* log a notice and the final one
    /// exits the process, as does the first `SIGTERM`. Returns immediately; the
    /// watcher runs until the process ends. Ports Go
    /// `InterceptInterruptSignals`.
    ///
    /// # Panics
    ///
    /// Must be called inside a [`tokio::task::LocalSet`]: the watcher is queued
    /// with `spawn_local`, which panics outside one.
    pub fn intercept_interrupt_signals(self: &Rc<Self>) {
        // Subscribed here rather than inside the watcher: the disposition has to
        // be taken over before the caller's first await, and a subscription
        // renewed per iteration would miss a signal arriving while the previous
        // one is still being handled.
        let mut interrupts = match Interrupts::subscribe() {
            Ok(interrupts) => interrupts,
            Err(e) => {
                // Not verbose-only: the run continues without interrupt
                // handling, which is a degraded mode for its whole lifetime.
                self.logger().borrow_mut().warnf(&format!(
                    "cannot intercept interrupts, signals will not be handled: {e}\n"
                ));
                return;
            }
        };
        let this = Rc::clone(self);
        tokio::task::spawn_local(async move {
            for i in 0..MAX_INTERRUPT_SIGNALS {
                let Ok(next) = interrupts.next().await else {
                    // The same degraded mode as failing to subscribe: without a
                    // signal to await, this would spin, reporting arrivals that
                    // never happened.
                    this.logger()
                        .borrow_mut()
                        .warnf("interrupt handling stopped, signals will not be handled\n");
                    return;
                };
                // tokio's signal stream is a notification rather than a queue,
                // so signals arriving faster than this loop is polled coalesce
                // into one. Go's buffered channel delivers each, so a very fast
                // triple Ctrl-C can need a fourth press here.
                //
                // A `SIGTERM` is a supervisor asking the process to stop, and it
                // reached only this process. Nothing is gained by waiting for a
                // second one that a supervisor will not send before its
                // `SIGKILL`, so it is acted on where it arrives.
                if i.saturating_add(1) >= MAX_INTERRUPT_SIGNALS
                    || matches!(next, crate::reap::Stop::Terminate)
                {
                    let nth = if matches!(next, crate::reap::Stop::Terminate) {
                        ""
                    } else {
                        " for the third time"
                    };
                    this.logger().borrow_mut().errf(
                        Color::Red,
                        &format!(
                            "task: Signal received{nth}: {:?}. Forcing shutdown\n",
                            signal_name(next)
                        ),
                    );
                    // Nothing gets to clean up after this, so stop the commands
                    // before the process goes. Swept without awaiting: yielding
                    // here would let the run this is cutting short finish and
                    // return its own exit code instead of the forced one.
                    //
                    // What the exit skips, beyond the usual destructors: a cache
                    // lock is left to its TTL rather than released, and output
                    // buffered by `--output group` is discarded. The logger
                    // itself is safe — it writes through to the streams.
                    let swept = crate::reap::stop_commands_blocking();
                    this.report_swept(swept, "still running at the forced shutdown");
                    // Go exits 1 here rather than with the "cancelled" code; the
                    // run never got to produce a code of its own.
                    std::process::exit(1);
                }
                this.logger().borrow_mut().outf(
                    Color::Yellow,
                    &format!("task: Signal received: {:?}\n", signal_name(next)),
                );
                // Not on the first: a terminal has already delivered it to the
                // group, and signalling again would interrupt a command's own
                // handler mid-cleanup. Asked twice, pass it on — and a `task`
                // signalled by pid never delivered anything to its commands.
                if i > 0 {
                    let swept = crate::reap::signal_commands(next);
                    this.report_swept(swept, "still running, passing the signal on");
                }
            }
        });
    }
}

/// The interrupt signals the engine takes over, subscribed once so that none is
/// missed between two of them.
struct Interrupts {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl Interrupts {
    /// Takes over the signals. Returns `Err` if the handlers cannot be
    /// installed.
    fn subscribe() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                sigint: signal(SignalKind::interrupt())?,
                sigterm: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    /// Awaits the next one, reporting which arrived so it can be passed on
    /// unchanged. Returns `Err` if there is no longer a signal to await, which
    /// must not be read as one having arrived.
    async fn next(&mut self) -> std::io::Result<crate::reap::Stop> {
        #[cfg(unix)]
        {
            Ok(tokio::select! {
                _ = self.sigint.recv() => crate::reap::Stop::Interrupt,
                _ = self.sigterm.recv() => crate::reap::Stop::Terminate,
            })
        }
        #[cfg(not(unix))]
        {
            // Windows has no signals to distinguish; Ctrl-C is the only one that
            // arrives here, and passing it on terminates the command outright.
            //
            // Unlike the Unix arm this subscribes per call, since
            // `tokio::signal::ctrl_c` has no stream to hold: a Ctrl-C arriving
            // while the previous one is handled can be missed, and a failure to
            // register returns at once — which is why it is not read as an
            // arrival.
            tokio::signal::ctrl_c()
                .await
                .map(|()| crate::reap::Stop::Interrupt)
        }
    }
}

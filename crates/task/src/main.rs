//! The `task` command-line entry point: a thin CLI over `taskcore`.
//!
//! Parses the flag set with `clap`, wires the parsed flags into a
//! [`taskcore::executor::Executor`], and drives its async API on a
//! current-thread tokio runtime inside a [`tokio::task::LocalSet`] (the engine
//! internals are `!Send`). Handles task listing, `--init`, `--completion`, a
//! line-based [`Prompter`], and fuzzy "did you mean" suggestions.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects
    )
)]

mod cli;
mod fuzzy;
mod init;
mod prompter;
mod run;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Install the ring crypto provider before anything touches TLS (reqwest,
    // the OCI/vk cache). Erroring here only means a provider is already
    // installed, which is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();

    match run::run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err}");
            ExitCode::from(err.exit_code())
        }
    }
}

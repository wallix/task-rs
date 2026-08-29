//! Black-box tests for the command-line interface itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::{run, stage, testdata};

#[test]
fn help_lists_every_option() {
    let dir = stage("help");
    let out = run(&dir, &["--help"]);

    assert!(out.ok(), "help failed: {}", out.combined());
    let expected = std::fs::read_to_string(testdata("help").join("help.txt")).unwrap();
    assert_eq!(out.stdout, expected);
    assert!(out.stderr.is_empty(), "unexpected stderr: {}", out.stderr);
}

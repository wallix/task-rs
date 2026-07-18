//! The `task` runner library: Taskfile parsing, templating, shell execution,
//! variable compilation, and the DAG execution engine with caching and watch.
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

pub mod ast;
pub mod cache;
pub mod call;
pub mod compiler;
pub mod concurrency;
pub mod editors;
pub mod env;
pub mod execext;
pub mod executor;
pub mod filepathext;
pub mod fingerprint;
pub mod goext;
pub mod hash;
pub mod logger;
pub mod migrate;
pub mod output;
pub mod precondition;
pub mod reader;
pub mod requires;
pub mod slicesext;
pub mod sort;
pub mod summary;
pub mod sysinfo;
pub mod templater;
pub mod term;
pub mod variables;
pub mod version;

//! Reading and resolving Taskfiles from their sources.
//!
//! This module turns a [`Node`] (a file or stdin) into a fully resolved
//! [`ast::TaskfileGraph`](crate::ast::TaskfileGraph): it parses YAML, records
//! source locations, recurses through `includes`, and detects cycles. It also
//! provides `.env` loading and error-snippet extraction used to build readable
//! diagnostics.

mod dotenv;
mod error;
mod fsext;
mod node;
mod read;
mod snippet;

pub use dotenv::dotenv;
pub use error::{ReaderError, TaskfileNotFoundError};
pub use node::{FileNode, Node, StdinNode, new_node, new_root_node};
pub use read::{DebugFunc, PromptFunc, Reader};
pub use snippet::{Snippet, SnippetOptions};

/// The Taskfile file names searched for by default, in priority order.
pub const DEFAULT_TASKFILES: [&str; 8] = [
    "Taskfile.yml",
    "taskfile.yml",
    "Taskfile.yaml",
    "taskfile.yaml",
    "Taskfile.dist.yml",
    "taskfile.dist.yml",
    "Taskfile.dist.yaml",
    "taskfile.dist.yaml",
];

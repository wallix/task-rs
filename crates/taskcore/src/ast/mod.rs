//! The abstract syntax tree for a Taskfile.
//!
//! Each submodule ports one type family from the Go `taskfile/ast` package,
//! preserving the exact set of YAML shapes each type accepts. The insertion
//! order of `vars`, `tasks`, `includes`, and matrix rows is load-bearing, so
//! those maps are backed by [`indexmap::IndexMap`].

mod cache;
mod cmd;
mod defer;
mod dep;
mod dialect;
mod error;
mod for_;
mod glob;
mod graph;
mod include;
mod location;
mod matrix;
mod output;
mod platforms;
mod precondition;
mod prompt;
mod requires;
mod task;
mod taskfile;
mod tasks;
mod var;
mod vars;

pub use cache::{Cache, Caches};
pub use cmd::Cmd;
pub use defer::Defer;
pub use dep::Dep;
pub use dialect::Dialect;
pub use error::TaskfileDecodeError;
pub use for_::For;
pub use glob::Glob;
pub use graph::{TaskfileGraph, TaskfileVertex};
pub use include::{Include, IncludeElement, Includes};
pub use location::Location;
pub use matrix::{Matrix, MatrixElement, MatrixRow};
pub use output::{Output, OutputGroup};
pub use platforms::{ErrInvalidPlatform, Platform};
pub use precondition::Precondition;
pub use prompt::Prompt;
pub use requires::{Requires, VarsWithValidation};
pub use task::Task;
pub use taskfile::{NAMESPACE_SEPARATOR, Taskfile};
pub use tasks::{TaskElement, Tasks};
pub use var::Var;
pub use vars::{VarElement, Vars};

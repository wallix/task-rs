//! JSON output for editor integrations (`task --list-all --json`).
//!
//! Ports the Go `internal/editors` package: the exact shape the VS Code and
//! JetBrains Task plugins consume. `--nested` groups tasks into a tree of
//! namespaces split on `:`; the flat form lists every task under `tasks`.

use std::collections::BTreeMap;

use serde::Serialize;

/// A namespace of tasks, optionally containing child namespaces.
#[derive(Serialize, Default)]
pub struct Namespace {
    /// The tasks directly in this namespace.
    pub tasks: Vec<EditorTask>,
    /// Child namespaces, keyed by their segment (only in `--nested` mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<BTreeMap<String, Namespace>>,
    /// The root Taskfile location (root namespace only).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub location: String,
}

/// A single task as exposed to editors.
#[derive(Serialize)]
pub struct EditorTask {
    /// The display name (label, full name, or key).
    pub name: String,
    /// The raw task key.
    pub task: String,
    /// The description.
    pub desc: String,
    /// The summary text.
    pub summary: String,
    /// The task's aliases.
    pub aliases: Vec<String>,
    /// Whether the task's fingerprint is up to date, when it could be computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_to_date: Option<bool>,
    /// Where the task is defined.
    pub location: Option<EditorLocation>,
}

/// A task's location in a Taskfile.
#[derive(Serialize)]
pub struct EditorLocation {
    /// 1-based line.
    pub line: usize,
    /// 1-based column.
    pub column: usize,
    /// The Taskfile path.
    pub taskfile: String,
}

impl Namespace {
    /// Inserts `task` at the namespace path derived from splitting its raw name
    /// on `:`. Ports Go `Namespace.AddNamespace`: a single-segment path is a
    /// task in this namespace; a longer path recurses into a child namespace.
    pub fn add_namespaced(&mut self, path: &[&str], task: EditorTask) {
        match path {
            [] => {}
            [_] => self.tasks.push(task),
            [head, rest @ ..] => {
                let child = self
                    .namespaces
                    .get_or_insert_with(BTreeMap::new)
                    .entry((*head).to_string())
                    .or_default();
                child.add_namespaced(rest, task);
            }
        }
    }
}

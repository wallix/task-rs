//! Recursive reading of a Taskfile and its transitive includes into a
//! [`TaskfileGraph`].
//!
//! Starting from a root [`Node`], the reader parses each Taskfile, then for
//! every `include` it resolves the entrypoint/dir relative to the including
//! node, reads the included Taskfile, and records an edge. Edges carry the list
//! of includes that produced them so the graph can be merged later. Cycles are
//! rejected as [`ReaderError::Cycle`].

use crate::ast;
use crate::env;
use crate::filepathext;
use crate::templater;

use super::error::ReaderError;
use super::node::{Node, new_node};
use super::snippet::{Snippet, SnippetOptions};

/// A callback invoked with debug messages emitted while reading.
pub type DebugFunc = Box<dyn Fn(&str) + Send + Sync>;
/// A callback invoked to prompt the user; returning `Err` rejects the prompt.
pub type PromptFunc = Box<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Reads Taskfiles recursively from a [`Node`], building a [`TaskfileGraph`].
#[derive(Default)]
pub struct Reader {
    graph: ast::TaskfileGraph,
    temp_dir: Option<String>,
    debug_func: Option<DebugFunc>,
    prompt_func: Option<PromptFunc>,
}

impl Reader {
    /// Creates a reader with default options.
    pub fn new() -> Self {
        Reader::default()
    }

    /// Sets the temporary directory used by the reader.
    pub fn with_temp_dir(mut self, temp_dir: impl Into<String>) -> Self {
        self.temp_dir = Some(temp_dir.into());
        self
    }

    /// Sets the debug callback.
    pub fn with_debug_func(mut self, f: DebugFunc) -> Self {
        self.debug_func = Some(f);
        self
    }

    /// Sets the prompt callback.
    pub fn with_prompt_func(mut self, f: PromptFunc) -> Self {
        self.prompt_func = Some(f);
        self
    }

    /// The temporary directory the reader will use, if one was configured.
    pub fn temp_dir(&self) -> Option<&str> {
        self.temp_dir.as_deref()
    }

    fn debug(&self, msg: &str) {
        if let Some(f) = &self.debug_func {
            f(msg);
        }
    }

    /// Reads the Taskfile at `node`, recursing through its includes, and
    /// returns the built graph.
    pub fn read(mut self, node: &dyn Node) -> Result<ast::TaskfileGraph, ReaderError> {
        self.include(node)?;
        Ok(self.graph)
    }

    fn include(&mut self, node: &dyn Node) -> Result<(), ReaderError> {
        let location = node.location().to_string();

        // A vertex already present means this Taskfile was read and its
        // children explored, so there is nothing more to do.
        if self.graph.vertex(&location).is_some() {
            return Ok(());
        }

        self.debug(&format!("reading taskfile: {location}"));
        let taskfile = self.read_node(node)?;

        self.graph.add_vertex(ast::TaskfileVertex {
            uri: location.clone(),
            taskfile: taskfile.clone(),
        });

        // Environment plus this Taskfile's own vars form the templating context
        // for resolving each include's fields.
        let mut vars = env::get_environ();
        vars.merge(&taskfile.vars, None);

        for (_, include) in taskfile.includes.all() {
            let mut cache = templater::Cache::new(vars.clone());
            cache.set_dialect(taskfile.templater);
            let mut include = ast::Include {
                namespace: include.namespace.clone(),
                taskfile: cache.replace(&include.taskfile),
                dir: cache.replace(&include.dir),
                optional: include.optional,
                internal: include.internal,
                flatten: include.flatten,
                aliases: include.aliases.clone(),
                excludes: include.excludes.clone(),
                advanced_import: include.advanced_import,
                vars: include.vars.clone(),
            };
            if let Some(e) = cache.err() {
                return Err(ReaderError::Template(e.to_string()));
            }

            let entrypoint = node.resolve_entrypoint(&include.taskfile)?;
            include.dir = node.resolve_dir(&include.dir)?;

            let include_node = match new_node(&entrypoint, &include.dir) {
                Ok(n) => n,
                Err(e) => {
                    if include.optional {
                        continue;
                    }
                    return Err(e);
                }
            };

            let include_location = include_node.location().to_string();

            // Recurse before recording the edge, matching the Go reader.
            self.include(include_node.as_ref())?;

            self.add_edge(&location, &include_location, include)?;
        }

        Ok(())
    }

    /// Records an edge from `source` to `target`, appending to any existing
    /// edge's include list. Cycles are rejected.
    fn add_edge(
        &mut self,
        source: &str,
        target: &str,
        include: ast::Include,
    ) -> Result<(), ReaderError> {
        match self.graph.add_edge(source, target, vec![include]) {
            Ok(()) => Ok(()),
            Err(_) => {
                // add_edge fails when the edge already exists (merged into the
                // existing include list by the graph) or would create a cycle.
                // The graph reports a cycle in its message.
                Err(ReaderError::Cycle {
                    source: source.to_string(),
                    destination: target.to_string(),
                })
            }
        }
    }

    fn read_node(&self, node: &dyn Node) -> Result<ast::Taskfile, ReaderError> {
        let b = node.read()?;
        let location = node.location().to_string();

        let mut tf: ast::Taskfile = match serde_yaml_ng::from_slice(&b) {
            Ok(tf) => tf,
            Err(e) => {
                if let Some(loc) = e.location() {
                    let snippet = Snippet::new(
                        &b,
                        SnippetOptions {
                            line: loc.line(),
                            column: loc.column(),
                            padding: 2,
                            no_indicators: false,
                        },
                    );
                    return Err(ReaderError::Invalid {
                        uri: location,
                        err: format!("{e}\n{}", snippet.render()),
                    });
                }
                return Err(ReaderError::Invalid {
                    uri: filepathext::try_abs_to_rel(&location)
                        .to_string_lossy()
                        .into_owned(),
                    err: e.to_string(),
                });
            }
        };

        // A Taskfile must declare a schema version.
        if tf.version.is_none() {
            return Err(ReaderError::MissingVersion { uri: location });
        }

        // Resolve the effective template dialect. An explicit `templater:` field
        // always wins; otherwise it is detected from the syntax, defaulting to
        // Jinja when the file has no distinctly-Go templates.
        if !tf.templater_explicit {
            let src = std::str::from_utf8(&b).unwrap_or_default();
            tf.templater = templater::detect_dialect(src);
        }

        // Stamp the source location and template dialect onto the Taskfile and
        // each task. The dialect is per-file, so a task and its variables carry
        // the dialect of the Taskfile that defined them even after includes are
        // merged into another map.
        tf.location = location.clone();
        let dialect = tf.templater;
        tf.vars.set_dialect(dialect);
        tf.env.set_dialect(dialect);
        // Recover each task's line/column by scanning the source: decoding
        // through `serde_yaml_ng::Value` loses YAML node positions, which editor
        // integrations need to jump to a task definition.
        let keys: Vec<String> = tf.tasks.iter().map(|(k, _)| k.clone()).collect();
        let positions = scan_task_positions(&b, &keys);
        for key in keys {
            if let Some(task) = tf.tasks.get_mut(&key) {
                task.dialect = dialect;
                let pos = positions.get(&key).copied();
                if let Some(v) = &mut task.vars {
                    v.set_dialect(dialect);
                }
                if let Some(v) = &mut task.env {
                    v.set_dialect(dialect);
                }
                for cmd in &mut task.cmds {
                    if let Some(v) = &mut cmd.vars {
                        v.set_dialect(dialect);
                    }
                }
                for dep in task.deps.iter_mut().chain(task.setup.iter_mut()) {
                    if let Some(v) = &mut dep.vars {
                        v.set_dialect(dialect);
                    }
                }
                let loc = task.location.get_or_insert_with(ast::Location::default);
                if loc.taskfile.is_empty() {
                    loc.taskfile = location.clone();
                }
                if let Some((line, column)) = pos {
                    loc.line = line;
                    loc.column = column;
                }
            }
        }

        Ok(tf)
    }
}

/// Scans the raw Taskfile source for each task key's 1-based line and column.
///
/// Only lines inside the top-level `tasks:` block are considered, and a key is
/// matched at the shallowest child indentation (a mapping key ending in `:`),
/// so a task's nested fields (`desc:`, `cmds:`, …) are not mistaken for tasks.
/// A key whose position cannot be found (unusual quoting, flow style) is simply
/// absent from the map, leaving its location at the default 0/0.
fn scan_task_positions(
    src: &[u8],
    keys: &[String],
) -> std::collections::HashMap<String, (usize, usize)> {
    use std::collections::HashSet;
    let text = std::str::from_utf8(src).unwrap_or_default();
    let wanted: HashSet<&str> = keys.iter().map(String::as_str).collect();
    let mut out = std::collections::HashMap::new();

    let mut in_tasks = false;
    let mut task_indent: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len().saturating_sub(trimmed.len());
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Top-level keys (no indentation) open or close the `tasks:` block.
        if indent == 0 {
            in_tasks = trimmed.starts_with("tasks:");
            task_indent = None;
            continue;
        }
        if !in_tasks {
            continue;
        }
        // The first indented key under `tasks:` fixes the task-key indentation;
        // deeper lines are task bodies, not task names.
        let depth = *task_indent.get_or_insert(indent);
        if indent != depth {
            continue;
        }
        // The mapping key ends at the first `:` followed by whitespace or the end
        // of the line, so a namespaced key like `ns:sub:` is read whole.
        let bytes = trimmed.as_bytes();
        let Some(sep) = (0..bytes.len()).find(|&idx| {
            bytes.get(idx) == Some(&b':')
                && matches!(
                    bytes.get(idx.saturating_add(1)),
                    None | Some(b' ') | Some(b'\t')
                )
        }) else {
            continue;
        };
        let key = trimmed
            .get(..sep)
            .unwrap_or_default()
            .trim_matches(['"', '\'']);
        if wanted.contains(key) {
            out.entry(key.to_string())
                .or_insert((i.saturating_add(1), indent.saturating_add(1)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::scan_task_positions;

    #[test]
    fn scans_task_line_and_column() {
        let src = b"version: '3'\n\n# a comment\ntasks:\n  build:\n    desc: B\n    cmds: ['x']\n  ns:sub:\n    cmds: ['y']\nenv:\n  X: 1\n";
        let keys = vec!["build".to_string(), "ns:sub".to_string()];
        let pos = scan_task_positions(src, &keys);
        assert_eq!(pos.get("build"), Some(&(5, 3)));
        // Namespaced key read whole, not split at the first colon.
        assert_eq!(pos.get("ns:sub"), Some(&(8, 3)));
    }

    #[test]
    fn ignores_task_body_and_other_sections() {
        // `desc`/`cmds` are task bodies, and `X` lives under `env:`, so none are
        // mistaken for tasks even though they are `key:` lines.
        let src = b"version: '3'\ntasks:\n  build:\n    desc: B\nenv:\n  X: 1\n";
        let keys = vec!["build".to_string(), "desc".to_string(), "X".to_string()];
        let pos = scan_task_positions(src, &keys);
        assert_eq!(pos.get("build"), Some(&(3, 3)));
        assert!(!pos.contains_key("desc"));
        assert!(!pos.contains_key("X"));
    }
}

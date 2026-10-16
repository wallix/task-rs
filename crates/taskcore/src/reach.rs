//! What a `task:` or `from:` entry of `sources`, `generates` or
//! `sandbox.writes` reaches.
//!
//! A deferred `task:` command is compiled unrendered, so that `EXIT_CODE` is
//! known when it runs. [`cmd_calls`] renders it as it runs after a success.

use crate::ast::{Cmd, Task, Var, Vars};
use crate::templater::{Cache, TemplaterError};
use crate::variables::{CompileError, DeferredCall};
use crate::yaml::Value;

/// The `task:` commands of `t`, as `from: cmds` and a run see them: deferred
/// ones rendered as on success; one whose name renders empty runs nothing.
/// `has_refs` when `t` has `from:` or `task:` entries in `sources` or
/// `generates`: its final `CHECKSUM` is not known yet. The error leaves the
/// referrer and field for the caller to fill in.
pub(crate) fn cmd_calls(t: &Task, has_refs: bool) -> Result<Vec<(String, Vars)>, CompileError> {
    let mut out = Vec::new();
    for c in &t.cmds {
        let (name, vars) = if c.defer {
            for (var, refused) in [("EXIT_CODE", true), ("CHECKSUM", has_refs)] {
                if refused && mentions(c, var) {
                    return Err(CompileError::DeferredCallUnsupported(Box::new(
                        DeferredCall {
                            task: String::new(),
                            field: String::new(),
                            via: Vec::new(),
                            owner: t.name().to_string(),
                            template: c.task.clone(),
                            var: var.to_string(),
                        },
                    )));
                }
            }
            deferred_call(t, c).map_err(CompileError::Template)?
        } else {
            (c.task.clone(), c.vars.clone())
        };
        if !name.is_empty() {
            out.push((name, vars.unwrap_or_default()));
        }
    }
    Ok(out)
}

/// Renders a deferred command's `task:` name and vars as `run_deferred` does
/// when the task succeeded: from the task's final vars, without `EXIT_CODE`.
pub(crate) fn deferred_call(t: &Task, c: &Cmd) -> Result<(String, Option<Vars>), TemplaterError> {
    let mut cache = Cache::new(t.vars.clone().unwrap_or_default());
    cache.set_dialect(t.dialect);
    let name = cache.replace(&c.task);
    let vars = c.vars.as_ref().and_then(|v| cache.replace_vars(v));
    match cache.err() {
        Some(e) => Err(e.clone()),
        None => Ok((name, vars)),
    }
}

/// Whether a raw deferred call's name or vars mention `word`.
/// Uses substring matching, so false positives are possible.
fn mentions(c: &Cmd, word: &str) -> bool {
    fn value(v: &Value, word: &str) -> bool {
        match v {
            Value::String(s) => s.contains(word),
            Value::Sequence(seq) => seq.iter().any(|v| value(v, word)),
            Value::Mapping(map) => map.iter().any(|(k, v)| value(k, word) || value(v, word)),
            _ => false,
        }
    }
    let var = |v: &Var| {
        v.sh.as_deref().is_some_and(|s| s.contains(word))
            || v.ref_.contains(word)
            || v.value.as_ref().is_some_and(|v| value(v, word))
    };
    c.task.contains(word) || c.vars.iter().flat_map(Vars::all).any(|(_, v)| var(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deferred call is rendered as on success; one that needs EXIT_CODE
    /// is refused, and one that needs CHECKSUM when the task has references.
    #[test]
    fn deferred_calls_render_without_the_exit_code() {
        let mut t = Task {
            task: "x".to_string(),
            vars: Some(Vars::from_elements([crate::ast::VarElement {
                key: "T".to_string(),
                value: Var::from_string("y"),
            }])),
            ..Default::default()
        };
        let call = |name: &str| Cmd {
            task: name.to_string(),
            defer: true,
            ..Default::default()
        };
        t.cmds = vec![call("{{.T}}"), call("{{.NONE}}")];
        assert_eq!(
            cmd_calls(&t, false).unwrap(),
            [("y".to_string(), Vars::new())]
        );
        t.cmds = vec![call("{{if .EXIT_CODE}}a{{else}}b{{end}}")];
        assert!(matches!(
            cmd_calls(&t, false),
            Err(CompileError::DeferredCallUnsupported(_))
        ));
        t.cmds = vec![call("c-{{.CHECKSUM}}")];
        assert!(cmd_calls(&t, false).is_ok());
        assert!(matches!(
            cmd_calls(&t, true),
            Err(CompileError::DeferredCallUnsupported(_))
        ));
    }
}

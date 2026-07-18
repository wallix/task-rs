//! Interactive prompting for missing required variables. Ports the
//! `promptDepsVars`/`promptTaskVars` half of Go `requires.go`.

use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

use crate::ast::{Task, Var, Vars};
use crate::call::Call;
use crate::requires;

use super::{Executor, ExecutorError, PromptError};

impl Executor {
    /// Traverses the dependency tree of `calls`, collects every missing required
    /// variable, and prompts for them upfront — dependencies run in parallel, so
    /// all prompts must happen before execution to avoid interleaving. Prompted
    /// values are stored in `prompted_vars` for injection into task calls. A
    /// no-op when prompting is not possible.
    pub(crate) async fn prompt_deps_vars(
        self: &Rc<Self>,
        calls: &[Call],
    ) -> Result<(), ExecutorError> {
        if !self.can_prompt() {
            return Ok(());
        }

        let mut visited: HashSet<String> = HashSet::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut missing: Vec<requires::MissingVar> = Vec::new();
        let mut queue: VecDeque<Call> = calls.iter().cloned().collect();

        while let Some(call) = queue.pop_front() {
            let ct = self.fast_compiled_task(&call).await?;
            // Collect before the visited check, so the same task reached via a
            // different call still contributes its (deduplicated) missing vars.
            for v in requires::missing_required_vars(&ct) {
                if seen.insert(v.name.clone()) {
                    missing.push(v);
                }
            }
            if !visited.insert(call.task.clone()) {
                continue;
            }
            for dep in &ct.deps {
                queue.push_back(Call {
                    task: dep.task.clone(),
                    vars: dep.vars.clone().unwrap_or_default(),
                    silent: dep.silent,
                    indirect: false,
                });
            }
        }

        for v in &missing {
            let value = self.prompt_for(&v.name, &v.allowed_values)?;
            let mut pv = self.prompted_vars.borrow_mut();
            pv.get_or_insert_with(Vars::new)
                .set(v.name.clone(), Var::from_string(value));
        }
        Ok(())
    }

    /// Prompts for a single task's missing required vars just-in-time (for
    /// sequential commands), adding them to `call.vars` and caching them in
    /// `prompted_vars`. Returns whether anything was prompted, so the caller can
    /// recompile the task with the new values. Ports Go `promptTaskVars`.
    pub(crate) fn prompt_task_vars(
        &self,
        t: &Task,
        call: &mut Call,
    ) -> Result<bool, ExecutorError> {
        if !self.can_prompt() {
            return Ok(false);
        }

        let mut prompted = false;
        for v in requires::missing_required_vars(t) {
            let already = self
                .prompted_vars
                .borrow()
                .as_ref()
                .is_some_and(|pv| pv.get(&v.name).is_some());
            if already {
                continue;
            }
            let value = self.prompt_for(&v.name, &v.allowed_values)?;
            call.vars
                .set(v.name.clone(), Var::from_string(value.clone()));
            self.prompted_vars
                .borrow_mut()
                .get_or_insert_with(Vars::new)
                .set(v.name.clone(), Var::from_string(value));
            prompted = true;
        }
        Ok(prompted)
    }

    /// Asks the configured prompter for a variable's value, mapping a cancelled
    /// prompt to [`ExecutorError::Cancelled`].
    fn prompt_for(&self, name: &str, enum_values: &[String]) -> Result<String, ExecutorError> {
        let prompter = self
            .prompter
            .as_ref()
            .ok_or_else(|| ExecutorError::Io("no prompter available".to_string()))?;
        prompter.prompt(name, enum_values).map_err(|e| match e {
            PromptError::Cancelled => ExecutorError::Cancelled,
            other => ExecutorError::Io(format!("prompt failed: {other}")),
        })
    }
}

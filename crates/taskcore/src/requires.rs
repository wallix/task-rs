//! Validation of a task's required variables.
//!
//! A task may declare `requires.vars`, listing variable names that must be set
//! (optionally constrained to an enum of allowed values). This module reports
//! which required vars are missing and whether any set value falls outside its
//! allowed enum. The interactive prompting paths from the Go `requires.go`
//! (`promptDepsVars`/`promptTaskVars`) depend on the Executor and its
//! `input.Prompter`, so they are deferred to the executor port; only the pure
//! validation is here.

use serde_yaml_ng::Value;

use crate::ast::{Task, VarsWithValidation};

/// A required variable that was not set, with its allowed values (if any).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingVar {
    /// The variable name.
    pub name: String,
    /// The allowed values, empty when unconstrained.
    pub allowed_values: Vec<String>,
}

impl MissingVar {
    /// Formats the variable for an error message, appending its allowed values
    /// when it is enum-constrained. Mirrors the Go `MissingVar.String`.
    fn display(&self) -> String {
        if self.allowed_values.is_empty() {
            self.name.clone()
        } else {
            format!(
                "{} (allowed values: {})",
                self.name,
                format_list(&self.allowed_values)
            )
        }
    }
}

/// A required variable whose set value is not one of its allowed enum values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotAllowedVar {
    /// The variable name.
    pub name: String,
    /// The value that was supplied.
    pub value: String,
    /// The allowed values.
    pub enum_values: Vec<String>,
}

/// An error raised when required-variable validation fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequiresError {
    /// One or more required variables were not set.
    MissingVars {
        /// The task name.
        task_name: String,
        /// The variables that were missing.
        missing: Vec<MissingVar>,
    },
    /// One or more set variables fell outside their allowed enum.
    NotAllowedVars {
        /// The task name.
        task_name: String,
        /// The offending variables.
        not_allowed: Vec<NotAllowedVar>,
    },
}

impl std::fmt::Display for RequiresError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingVars { task_name, missing } => {
                let vars: Vec<String> = missing.iter().map(MissingVar::display).collect();
                write!(
                    f,
                    "task: Task \"{task_name}\" cancelled because it is missing required variables: {}",
                    vars.join(", ")
                )
            }
            Self::NotAllowedVars {
                task_name,
                not_allowed,
            } => {
                writeln!(
                    f,
                    "task: Task \"{task_name}\" cancelled because it is missing required variables:"
                )?;
                for v in not_allowed {
                    writeln!(
                        f,
                        "  - {} has an invalid value : '{}' (allowed values : {})",
                        v.name,
                        v.value,
                        format_list(&v.enum_values)
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for RequiresError {}

/// Returns the required vars that are not present in the task's compiled vars.
pub fn missing_required_vars(task: &Task) -> Vec<MissingVar> {
    let Some(requires) = &task.requires else {
        return Vec::new();
    };
    let mut missing = Vec::new();
    for v in &requires.vars {
        let present = task
            .vars
            .as_ref()
            .is_some_and(|vars| vars.get(&v.name).is_some());
        if !present {
            missing.push(MissingVar {
                name: v.name.clone(),
                allowed_values: v.enum_values.clone(),
            });
        }
    }
    missing
}

/// Verifies that every required variable is set, returning an error naming the
/// missing ones otherwise. Ports Go `areTaskRequiredVarsSet`.
pub fn check_required_vars_set(task: &Task) -> Result<(), RequiresError> {
    let missing = missing_required_vars(task);
    if missing.is_empty() {
        return Ok(());
    }
    Err(RequiresError::MissingVars {
        task_name: task.name().to_string(),
        missing,
    })
}

/// Verifies that every enum-constrained required variable holds an allowed
/// value, returning an error listing the violations otherwise. A variable whose
/// value is not a string, or that has no enum, is not checked. Ports Go
/// `areTaskRequiredVarsAllowedValuesSet`.
pub fn check_allowed_values(task: &Task) -> Result<(), RequiresError> {
    let Some(requires) = &task.requires else {
        return Ok(());
    };
    if requires.vars.is_empty() {
        return Ok(());
    }

    let mut not_allowed = Vec::new();
    for required in &requires.vars {
        let Some(value) = string_value(task, required) else {
            continue;
        };
        if !required.enum_values.is_empty() && !required.enum_values.contains(&value) {
            not_allowed.push(NotAllowedVar {
                name: required.name.clone(),
                value,
                enum_values: required.enum_values.clone(),
            });
        }
    }

    if not_allowed.is_empty() {
        return Ok(());
    }
    Err(RequiresError::NotAllowedVars {
        task_name: task.name().to_string(),
        not_allowed,
    })
}

/// Returns the string value of the named required variable in the task's vars,
/// or `None` when it is unset or not a string.
fn string_value(task: &Task, required: &VarsWithValidation) -> Option<String> {
    let var = task.vars.as_ref()?.get(&required.name)?;
    match &var.value {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Formats a list the way Go's `fmt.Sprintf("%v", slice)` does: space-separated
/// inside square brackets.
fn format_list(values: &[String]) -> String {
    format!("[{}]", values.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Requires, Var, VarElement, Vars};

    fn string_var(v: &str) -> Var {
        Var {
            value: Some(Value::String(v.to_string())),
            ..Default::default()
        }
    }

    fn task_with(requires: Vec<VarsWithValidation>, set: &[(&str, &str)]) -> Task {
        let vars = Vars::from_elements(set.iter().map(|(k, v)| VarElement {
            key: (*k).to_string(),
            value: string_var(v),
        }));
        Task {
            task: "t".to_string(),
            requires: Some(Requires { vars: requires }),
            vars: Some(vars),
            ..Default::default()
        }
    }

    fn plain(name: &str) -> VarsWithValidation {
        VarsWithValidation {
            name: name.to_string(),
            enum_values: Vec::new(),
        }
    }

    fn enumed(name: &str, values: &[&str]) -> VarsWithValidation {
        VarsWithValidation {
            name: name.to_string(),
            enum_values: values.iter().map(|v| (*v).to_string()).collect(),
        }
    }

    #[test]
    fn no_requires_is_ok() {
        let task = Task::default();
        assert!(check_required_vars_set(&task).is_ok());
        assert!(check_allowed_values(&task).is_ok());
        assert!(missing_required_vars(&task).is_empty());
    }

    #[test]
    fn missing_var_is_reported() {
        let task = task_with(vec![plain("FOO")], &[]);
        let err = check_required_vars_set(&task).unwrap_err();
        assert!(matches!(err, RequiresError::MissingVars { .. }));
        assert!(err.to_string().contains("missing required variables: FOO"));
    }

    #[test]
    fn set_var_passes() {
        let task = task_with(vec![plain("FOO")], &[("FOO", "bar")]);
        assert!(check_required_vars_set(&task).is_ok());
    }

    #[test]
    fn missing_enum_var_lists_allowed_values() {
        let task = task_with(vec![enumed("ENV", &["dev", "prod"])], &[]);
        let err = check_required_vars_set(&task).unwrap_err();
        assert!(err.to_string().contains("ENV (allowed values: [dev prod])"));
    }

    #[test]
    fn value_outside_enum_is_rejected() {
        let task = task_with(vec![enumed("ENV", &["dev", "prod"])], &[("ENV", "staging")]);
        let err = check_allowed_values(&task).unwrap_err();
        assert!(matches!(err, RequiresError::NotAllowedVars { .. }));
        assert!(err.to_string().contains("staging"));
        assert!(err.to_string().contains("allowed values : [dev prod]"));
    }

    #[test]
    fn value_inside_enum_is_allowed() {
        let task = task_with(vec![enumed("ENV", &["dev", "prod"])], &[("ENV", "dev")]);
        assert!(check_allowed_values(&task).is_ok());
    }

    #[test]
    fn non_string_value_is_not_enum_checked() {
        let mut task = task_with(vec![enumed("N", &["1", "2"])], &[]);
        let mut vars = Vars::new();
        vars.set(
            "N".to_string(),
            Var {
                value: Some(Value::Number(5.into())),
                ..Default::default()
            },
        );
        task.vars = Some(vars);
        assert!(check_allowed_values(&task).is_ok());
    }
}

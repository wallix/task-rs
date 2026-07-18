//! Building process environment variable lists from task-defined variables.
//!
//! Task environment variables are merged onto the inherited process
//! environment. By default a task variable takes priority over one already
//! present in the process environment; `TASK_X_ENV_PRECEDENCE=0` restores the
//! old behaviour, where the existing process value wins.

use serde_yaml_ng::Value;

use crate::ast;

const TASK_VAR_PREFIX: &str = "TASK_";

/// Returns all process environment variables encapsulated in an [`ast::Vars`].
pub fn get_environ() -> ast::Vars {
    let mut m = ast::Vars::new();
    for (key, val) in std::env::vars() {
        m.set(
            key,
            ast::Var {
                value: Some(Value::String(val)),
                ..Default::default()
            },
        );
    }
    m
}

/// Returns the `KEY=VALUE` environment list for a task's `env`, or `None` when
/// the task has no environment. `env_precedence` reports whether task
/// variables override existing process environment entries.
pub fn get(task: &ast::Task, env_precedence: bool) -> Option<Vec<String>> {
    task.env
        .as_ref()
        .map(|env| get_from_vars(env, env_precedence))
}

/// Merges the given variables onto the process environment, returning the full
/// `KEY=VALUE` list. Dynamic (`sh`) variables and non-scalar values are
/// skipped. When `env_precedence` is false, a variable that is already set in
/// the process environment is left untouched.
pub fn get_from_vars(env: &ast::Vars, env_precedence: bool) -> Vec<String> {
    let mut environ: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();

    for (k, v) in env.to_cache_map() {
        let Some(rendered) = allowed_value(&v) else {
            continue;
        };
        if !env_precedence && std::env::var_os(&k).is_some() {
            continue;
        }
        environ.push(format!("{k}={rendered}"));
    }

    environ
}

/// Renders a value for inclusion in the environment when its type is allowed
/// (string, bool, or number), or returns `None` to skip it. Mirrors Go's
/// `fmt.Sprintf("%v", …)` for the allowed scalar types.
fn allowed_value(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Returns the raw value of a `TASK_`-prefixed environment variable, or empty
/// when unset.
pub fn get_task_env(key: &str) -> String {
    std::env::var(format!("{TASK_VAR_PREFIX}{key}")).unwrap_or_default()
}

/// Returns the boolean value of a `TASK_`-prefixed env var. The second element
/// is true when the variable is set and parses to a valid boolean.
pub fn get_task_env_bool(key: &str) -> (bool, bool) {
    let v = get_task_env(key);
    if v.is_empty() {
        return (false, false);
    }
    match parse_go_bool(&v) {
        Some(b) => (b, true),
        None => (false, false),
    }
}

/// Returns the integer value of a `TASK_`-prefixed env var. The second element
/// is true when the variable is set and parses to a valid integer.
pub fn get_task_env_int(key: &str) -> (i64, bool) {
    let v = get_task_env(key);
    if v.is_empty() {
        return (0, false);
    }
    match v.parse::<i64>() {
        Ok(i) => (i, true),
        Err(_) => (0, false),
    }
}

/// Returns the string value of a `TASK_`-prefixed env var. The second element
/// is true when the variable is set to a non-empty value.
pub fn get_task_env_string(key: &str) -> (String, bool) {
    let v = get_task_env(key);
    let set = !v.is_empty();
    (v, set)
}

/// Returns the trimmed, comma-separated entries of a `TASK_`-prefixed env var,
/// or `None` when unset or empty after trimming.
pub fn get_task_env_string_slice(key: &str) -> Option<Vec<String>> {
    let v = get_task_env(key);
    if v.is_empty() {
        return None;
    }
    let result: Vec<String> = v
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    if result.is_empty() {
        return None;
    }
    Some(result)
}

/// Parses a boolean the way Go's `strconv.ParseBool` does: it accepts
/// `1`, `t`, `T`, `TRUE`, `true`, `True`, `0`, `f`, `F`, `FALSE`, `false`,
/// `False`.
fn parse_go_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Var, VarElement, Vars};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn scalar(v: Value) -> Var {
        Var {
            value: Some(v),
            ..Default::default()
        }
    }

    #[test]
    fn allowed_value_filters_by_type() {
        assert_eq!(
            allowed_value(&Value::String("x".into())),
            Some("x".to_string())
        );
        assert_eq!(allowed_value(&Value::Bool(true)), Some("true".to_string()));
        assert_eq!(
            allowed_value(&Value::Number(42.into())),
            Some("42".to_string())
        );
        assert_eq!(allowed_value(&Value::Null), None);
        assert_eq!(allowed_value(&Value::Sequence(vec![])), None);
    }

    #[test]
    fn get_from_vars_appends_scalars() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vars = Vars::from_elements([
            VarElement {
                key: "TASKCORE_ENV_TEST_A".to_string(),
                value: scalar(Value::String("1".into())),
            },
            VarElement {
                key: "TASKCORE_ENV_TEST_B".to_string(),
                value: scalar(Value::Bool(true)),
            },
        ]);
        let environ = get_from_vars(&vars, true);
        assert!(environ.iter().any(|e| e == "TASKCORE_ENV_TEST_A=1"));
        assert!(environ.iter().any(|e| e == "TASKCORE_ENV_TEST_B=true"));
    }

    #[test]
    fn get_from_vars_skips_dynamic() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let vars = Vars::from_elements([VarElement {
            key: "TASKCORE_ENV_TEST_DYN".to_string(),
            value: Var {
                sh: Some("echo hi".to_string()),
                ..Default::default()
            },
        }]);
        let environ = get_from_vars(&vars, true);
        assert!(
            !environ
                .iter()
                .any(|e| e.starts_with("TASKCORE_ENV_TEST_DYN="))
        );
    }

    #[test]
    fn env_precedence_controls_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "TASKCORE_ENV_TEST_PREC";
        // SAFETY: ENV_LOCK serializes env mutation across tests.
        unsafe {
            std::env::set_var(key, "from_process");
        }
        let vars = Vars::from_elements([VarElement {
            key: key.to_string(),
            value: scalar(Value::String("from_task".into())),
        }]);

        // Without precedence, the existing process value is kept.
        let without = get_from_vars(&vars, false);
        assert!(
            without
                .iter()
                .any(|e| e == "TASKCORE_ENV_TEST_PREC=from_process")
        );
        assert!(
            !without
                .iter()
                .any(|e| e == "TASKCORE_ENV_TEST_PREC=from_task")
        );

        // With precedence, the task value is appended.
        let with = get_from_vars(&vars, true);
        assert!(with.iter().any(|e| e == "TASKCORE_ENV_TEST_PREC=from_task"));

        unsafe {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn get_returns_none_without_env() {
        let task = ast::Task::default();
        assert!(get(&task, true).is_none());
    }

    #[test]
    fn task_env_helpers() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: ENV_LOCK serializes env mutation across tests.
        unsafe {
            std::env::set_var("TASK_TC_BOOL", "true");
            std::env::set_var("TASK_TC_INT", "7");
            std::env::set_var("TASK_TC_STR", "hello");
            std::env::set_var("TASK_TC_SLICE", "a, b ,,c");
            std::env::set_var("TASK_TC_BADBOOL", "maybe");
        }

        assert_eq!(get_task_env_bool("TC_BOOL"), (true, true));
        assert_eq!(get_task_env_bool("TC_MISSING"), (false, false));
        assert_eq!(get_task_env_bool("TC_BADBOOL"), (false, false));
        assert_eq!(get_task_env_int("TC_INT"), (7, true));
        assert_eq!(get_task_env_int("TC_STR"), (0, false));
        assert_eq!(get_task_env_string("TC_STR"), ("hello".to_string(), true));
        assert_eq!(get_task_env_string("TC_MISSING"), (String::new(), false));
        assert_eq!(
            get_task_env_string_slice("TC_SLICE"),
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
        assert_eq!(get_task_env_string_slice("TC_MISSING"), None);

        unsafe {
            std::env::remove_var("TASK_TC_BOOL");
            std::env::remove_var("TASK_TC_INT");
            std::env::remove_var("TASK_TC_STR");
            std::env::remove_var("TASK_TC_SLICE");
            std::env::remove_var("TASK_TC_BADBOOL");
        }
    }

    #[test]
    fn get_environ_captures_process_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let key = "TASKCORE_ENV_TEST_ENVIRON";
        // SAFETY: ENV_LOCK serializes env mutation across tests.
        unsafe {
            std::env::set_var(key, "value");
        }
        let vars = get_environ();
        assert_eq!(
            vars.get(key).and_then(|v| v.value.clone()),
            Some(Value::String("value".into()))
        );
        unsafe {
            std::env::remove_var(key);
        }
    }
}

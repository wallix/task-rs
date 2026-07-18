//! Prints a human-readable summary of a task: its name, description or summary
//! text, variables, environment, requirements, dependencies, aliases, and
//! commands.

use std::collections::HashSet;

use serde_yaml_ng::Value;

use crate::ast::{Task, Taskfile, Var};
use crate::logger::{Color, Logger};

/// Prints a summary for each of the named tasks, separated by blank lines.
pub fn print_tasks(l: &mut Logger, t: &Taskfile, calls: &[String]) {
    for (i, call) in calls.iter().enumerate() {
        print_space_between_summaries(l, i);
        if let Some(task) = t.tasks.get(call) {
            print_task(l, task);
        }
    }
}

/// Prints two blank lines before every summary except the first.
pub fn print_space_between_summaries(l: &mut Logger, i: usize) {
    if i == 0 {
        return;
    }
    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "\n");
}

/// Prints the full summary for a single task.
pub fn print_task(l: &mut Logger, t: &Task) {
    print_task_name(l, t);
    print_task_describing_text(l, t);
    print_task_vars(l, t);
    print_task_env(l, t);
    print_task_requires(l, t);
    print_task_dependencies(l, t);
    print_task_aliases(l, t);
    print_task_commands(l, t);
}

fn print_task_describing_text(l: &mut Logger, t: &Task) {
    if !t.summary.is_empty() {
        print_task_summary(l, t);
    } else if !t.desc.is_empty() {
        l.outf(Color::Default, &format!("{}\n", t.desc));
    } else {
        l.outf(
            Color::Default,
            "(task does not have description or summary)\n",
        );
    }
}

fn print_task_summary(l: &mut Logger, t: &Task) {
    let lines: Vec<&str> = t.summary.split('\n').collect();
    let count = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let not_last_line = i.saturating_add(1) < count;
        if not_last_line || !line.is_empty() {
            l.outf(Color::Default, &format!("{line}\n"));
        }
    }
}

fn print_task_name(l: &mut Logger, t: &Task) {
    l.outf(Color::Default, "task: ");
    l.outf(Color::Green, &format!("{}\n", t.name()));
    l.outf(Color::Default, "\n");
}

fn print_task_aliases(l: &mut Logger, t: &Task) {
    if t.aliases.is_empty() {
        return;
    }
    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "aliases:\n");
    for alias in &t.aliases {
        l.outf(Color::Default, " - ");
        l.outf(Color::Cyan, &format!("{alias}\n"));
    }
}

fn print_task_dependencies(l: &mut Logger, t: &Task) {
    if t.deps.is_empty() {
        return;
    }
    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "dependencies:\n");
    for d in &t.deps {
        l.outf(Color::Default, &format!(" - {}\n", d.task));
    }
}

fn print_task_commands(l: &mut Logger, t: &Task) {
    if t.cmds.is_empty() {
        return;
    }
    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "commands:\n");
    for c in &t.cmds {
        l.outf(Color::Default, " - ");
        if !c.cmd.is_empty() {
            l.outf(Color::Yellow, &format!("{}\n", c.cmd));
        } else {
            l.outf(Color::Green, &format!("Task: {}\n", c.task));
        }
    }
}

fn print_task_vars(l: &mut Logger, t: &Task) {
    let Some(vars) = &t.vars else { return };
    if vars.is_empty() {
        return;
    }

    let os_env_vars = get_env_var_names();

    let mut taskfile_env_vars: HashSet<String> = HashSet::new();
    if let Some(env) = &t.env {
        for (key, _) in env.all() {
            taskfile_env_vars.insert(key.clone());
        }
    }

    let displayable =
        |key: &str| !is_env_var(key, &os_env_vars) && !taskfile_env_vars.contains(key);

    if !vars.all().any(|(key, _)| displayable(key)) {
        return;
    }

    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "vars:\n");
    for (key, value) in vars.all() {
        if displayable(key) {
            let formatted = format_var_value(value);
            l.outf(Color::Yellow, &format!("  {key}: {formatted}\n"));
        }
    }
}

fn print_task_env(l: &mut Logger, t: &Task) {
    let Some(env) = &t.env else { return };
    if env.is_empty() {
        return;
    }

    let env_vars = get_env_var_names();
    let displayable = |key: &str| !is_env_var(key, &env_vars);

    if !env.all().any(|(key, _)| displayable(key)) {
        return;
    }

    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "env:\n");
    for (key, value) in env.all() {
        if displayable(key) {
            let formatted = format_var_value(value);
            l.outf(Color::Yellow, &format!("  {key}: {formatted}\n"));
        }
    }
}

/// Formats a variable value: shell command (`sh:`), reference (`ref:`), map, or
/// a quoted static scalar.
fn format_var_value(v: &Var) -> String {
    if let Some(sh) = &v.sh {
        return format!("sh: {sh}");
    }
    if !v.ref_.is_empty() {
        return format!("ref: {}", v.ref_);
    }
    match &v.value {
        Some(Value::Mapping(m)) => format_map(m, 4),
        Some(value) => format!("\"{}\"", scalar_to_string(value)),
        None => "\"\"".to_string(),
    }
}

/// Formats a mapping value with YAML-style indentation.
fn format_map(m: &serde_yaml_ng::Mapping, indent: usize) -> String {
    if m.is_empty() {
        return "{}".to_string();
    }
    let spaces = " ".repeat(indent);
    let mut result = String::from("\n");
    for (k, v) in m {
        let key = scalar_to_string(k);
        let value = scalar_to_string(v);
        result.push_str(&format!("{spaces}{key}: {value}\n"));
    }
    result
}

/// Renders a scalar YAML value the way Go's `%v` would for the corresponding
/// decoded type.
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => serde_yaml_ng::to_string(other)
            .unwrap_or_default()
            .trim_end()
            .to_string(),
    }
}

fn print_task_requires(l: &mut Logger, t: &Task) {
    let Some(requires) = &t.requires else { return };
    if requires.vars.is_empty() {
        return;
    }

    l.outf(Color::Default, "\n");
    l.outf(Color::Default, "requires:\n");
    l.outf(Color::Default, "  vars:\n");

    for v in &requires.vars {
        if v.enum_values.is_empty() {
            l.outf(Color::Yellow, &format!("    - {}\n", v.name));
        } else {
            l.outf(Color::Yellow, &format!("    - {}:\n", v.name));
            l.outf(Color::Yellow, "        enum:\n");
            for enum_value in &v.enum_values {
                l.outf(Color::Yellow, &format!("          - {enum_value}\n"));
            }
        }
    }
}

/// Collects the names of the current process environment variables.
fn get_env_var_names() -> HashSet<String> {
    std::env::vars().map(|(k, _)| k).collect()
}

/// Reports whether a variable is auto-generated by Task or comes from the OS
/// environment, in which case the summary hides it.
fn is_env_var(key: &str, env_vars: &HashSet<String>) -> bool {
    if key.starts_with("TASK_")
        || key.starts_with("CLI_")
        || key.starts_with("ROOT_")
        || key == "TASK"
        || key == "TASKFILE"
        || key == "TASKFILE_DIR"
        || key == "USER_WORKING_DIR"
        || key == "ALIAS"
        || key == "MATCH"
    {
        return true;
    }
    env_vars.contains(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Cmd, Dep, Tasks};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn dummy_logger() -> (SharedBuf, Logger) {
        let buf = SharedBuf::default();
        let l = Logger {
            stdout: Box::new(buf.clone()),
            stderr: Box::new(buf.clone()),
            ..Default::default()
        };
        (buf, l)
    }

    #[test]
    fn prints_dependencies_if_present() {
        let (buf, mut l) = dummy_logger();
        let task = Task {
            deps: vec![
                Dep {
                    task: "dep1".to_string(),
                    ..Default::default()
                },
                Dep {
                    task: "dep2".to_string(),
                    ..Default::default()
                },
                Dep {
                    task: "dep3".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        print_task(&mut l, &task);
        assert!(
            buf.contents()
                .contains("\ndependencies:\n - dep1\n - dep2\n - dep3\n")
        );
    }

    #[test]
    fn does_not_print_dependencies_if_missing() {
        let (buf, mut l) = dummy_logger();
        print_task(&mut l, &Task::default());
        assert!(!buf.contents().contains("dependencies:"));
    }

    #[test]
    fn prints_task_name() {
        let (buf, mut l) = dummy_logger();
        let task = Task {
            task: "my-task-name".to_string(),
            ..Default::default()
        };
        print_task(&mut l, &task);
        assert!(buf.contents().contains("task: my-task-name\n"));
    }

    #[test]
    fn prints_commands_if_present() {
        let (buf, mut l) = dummy_logger();
        let task = Task {
            cmds: vec![
                Cmd {
                    cmd: "command-1".to_string(),
                    ..Default::default()
                },
                Cmd {
                    cmd: "command-2".to_string(),
                    ..Default::default()
                },
                Cmd {
                    task: "task-1".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        print_task(&mut l, &task);
        let out = buf.contents();
        assert!(out.contains("\ncommands:\n"));
        assert!(out.contains("\n - command-1\n"));
        assert!(out.contains("\n - command-2\n"));
        assert!(out.contains("\n - Task: task-1\n"));
    }

    #[test]
    fn does_not_print_command_if_missing() {
        let (buf, mut l) = dummy_logger();
        print_task(&mut l, &Task::default());
        assert!(!buf.contents().contains("commands"));
    }

    #[test]
    fn layout() {
        let (buf, mut l) = dummy_logger();
        let task = Task {
            task: "sample-task".to_string(),
            summary: "line1\nline2\nline3\n".to_string(),
            deps: vec![Dep {
                task: "dependency".to_string(),
                ..Default::default()
            }],
            cmds: vec![Cmd {
                cmd: "command".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        print_task(&mut l, &task);
        let expected = "task: sample-task\n\nline1\nline2\nline3\n\ndependencies:\n - dependency\n\ncommands:\n - command\n";
        assert_eq!(buf.contents(), expected);
    }

    #[test]
    fn description_as_fallback() {
        let (buf, mut l) = dummy_logger();
        let without_summary = Task {
            desc: "description".to_string(),
            ..Default::default()
        };
        print_task(&mut l, &without_summary);
        assert!(buf.contents().contains("description"));

        let (buf, mut l) = dummy_logger();
        let with_summary = Task {
            desc: "description".to_string(),
            summary: "summary".to_string(),
            ..Default::default()
        };
        print_task(&mut l, &with_summary);
        assert!(!buf.contents().contains("description"));

        let (buf, mut l) = dummy_logger();
        print_task(&mut l, &Task::default());
        assert!(
            buf.contents()
                .contains("\n(task does not have description or summary)\n")
        );
    }

    #[test]
    fn print_all_with_spaces() {
        let (buf, mut l) = dummy_logger();
        let mut tasks = Tasks::new();
        for name in ["t1", "t2", "t3"] {
            tasks.set(
                name.to_string(),
                Task {
                    task: name.to_string(),
                    ..Default::default()
                },
            );
        }
        let tf = Taskfile {
            tasks,
            ..Default::default()
        };
        print_tasks(
            &mut l,
            &tf,
            &["t1".to_string(), "t2".to_string(), "t3".to_string()],
        );
        let out = buf.contents();
        assert!(out.starts_with("task: t1"));
        assert!(out.contains("\n(task does not have description or summary)\n\n\ntask: t2"));
        assert!(out.contains("\n(task does not have description or summary)\n\n\ntask: t3"));
    }
}

//! An insertion-ordered map of task names to [`Task`] definitions.

use indexmap::IndexMap;
use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_yaml_ng::Value;

use crate::sort::Sorter;

use super::error::TaskfileDecodeError;
use super::include::Include;
use super::location::Location;
use super::task::Task;
use super::taskfile::NAMESPACE_SEPARATOR;
use super::vars::Vars;

/// An ordered map of task names to tasks. Insertion order is preserved.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tasks {
    om: IndexMap<String, Task>,
}

/// A key-value pair used to initialize a [`Tasks`] map.
#[derive(Clone, Debug)]
pub struct TaskElement {
    pub key: String,
    pub value: Task,
}

impl Tasks {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a map initialized with the given elements, in order.
    pub fn from_elements(els: impl IntoIterator<Item = TaskElement>) -> Self {
        let mut tasks = Self::new();
        for el in els {
            tasks.set(el.key, el.value);
        }
        tasks
    }

    /// Returns the number of tasks.
    pub fn len(&self) -> usize {
        self.om.len()
    }

    /// Reports whether the map is empty.
    pub fn is_empty(&self) -> bool {
        self.om.is_empty()
    }

    /// Returns the task for `key`, if present.
    pub fn get(&self, key: &str) -> Option<&Task> {
        self.om.get(key)
    }

    /// Returns a mutable reference to the task for `key`, if present.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Task> {
        self.om.get_mut(key)
    }

    /// Inserts or updates the task for `key`, returning whether it existed.
    pub fn set(&mut self, key: String, value: Task) -> bool {
        self.om.insert(key, value).is_some()
    }

    /// Iterates over the pairs in the order dictated by `sorter`.
    pub fn all(&self, sorter: Sorter) -> Vec<(String, &Task)> {
        let mut keys: Vec<String> = self.om.keys().cloned().collect();
        sorter.sort(&mut keys, &[]);
        keys.into_iter()
            .filter_map(|k| self.om.get(&k).map(|t| (k.clone(), t)))
            .collect()
    }

    /// Iterates over the pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Task)> {
        self.om.iter()
    }

    /// Iterates over the keys in the order dictated by `sorter`.
    pub fn keys(&self, sorter: Sorter) -> Vec<String> {
        let mut keys: Vec<String> = self.om.keys().cloned().collect();
        sorter.sort(&mut keys, &[]);
        keys
    }

    /// Iterates over the values in the order dictated by `sorter`.
    pub fn values(&self, sorter: Sorter) -> Vec<&Task> {
        self.all(sorter).into_iter().map(|(_, t)| t).collect()
    }

    /// Merges the tasks of `other` into `self`, applying namespacing, exclude,
    /// flatten, and advanced-import rules from `include`. `included_taskfile_vars`
    /// carries the parent's variables so advanced imports can propagate them.
    pub fn merge(
        &mut self,
        other: &Tasks,
        include: &Include,
        included_taskfile_vars: &Vars,
    ) -> Result<(), String> {
        for (name, source) in other.iter() {
            // Deep copy so no data can be changed elsewhere once merged.
            let mut task = source.clone();
            // The task is internal if either the included task or the included
            // Taskfile is marked internal.
            task.internal = task.internal || include.internal;
            let mut task_name = name.clone();

            // A task in the exclude list is dropped.
            if include.excludes.iter().any(|e| e == name) {
                continue;
            }

            if !include.flatten {
                // Namespace the task's setup dependencies.
                for dep in &mut task.setup {
                    if !dep.task.is_empty() {
                        dep.task = task_name_with_namespace(&dep.task, &include.namespace);
                    }
                }
                // Namespace the task's dependencies.
                for dep in &mut task.deps {
                    if !dep.task.is_empty() {
                        dep.task = task_name_with_namespace(&dep.task, &include.namespace);
                    }
                }
                // Namespace the task's command calls.
                for cmd in &mut task.cmds {
                    if !cmd.task.is_empty() {
                        cmd.task = task_name_with_namespace(&cmd.task, &include.namespace);
                    }
                }
                // Namespace the existing aliases.
                for alias in &mut task.aliases {
                    *alias = task_name_with_namespace(alias, &include.namespace);
                }

                // Add namespace aliases derived from the include's aliases.
                for namespace_alias in &include.aliases {
                    task.aliases
                        .push(task_name_with_namespace(&task.task, namespace_alias));
                    for alias in &source.aliases {
                        task.aliases
                            .push(task_name_with_namespace(alias, namespace_alias));
                    }
                }

                task_name = task_name_with_namespace(name, &include.namespace);
                task.namespace = include.namespace.clone();
                task.task = task_name.clone();
            }

            if include.advanced_import {
                let mut dirs = Vec::with_capacity(task.dirs.len().saturating_add(1));
                dirs.push(include.dir.clone());
                dirs.append(&mut task.dirs);
                task.dirs = dirs;

                let mut include_vars = task.include_vars.take().unwrap_or_default();
                if let Some(vars) = &include.vars {
                    include_vars.merge(vars, None);
                }
                task.include_vars = Some(include_vars);
                task.included_taskfile_vars = Some(included_taskfile_vars.clone());
            }

            if self.get(&task_name).is_some() {
                return Err(format!(
                    "task: Found multiple tasks ({}) included by \"{}\"\"",
                    task_name, include.namespace
                ));
            }
            self.set(task_name, task);
        }

        // If the included Taskfile has a default task, is not flattened, and the
        // parent namespace has no matching task, alias the default so the user
        // can run it by namespace. Extend with the include's own aliases too.
        let t2_default_exists = other.get("default").is_some();
        let t1_namespace_exists = self.get(&include.namespace).is_some();
        if t2_default_exists && !t1_namespace_exists && !include.flatten {
            let default_task_name = format!("{}:default", include.namespace);
            if let Some(default_task) = self.get_mut(&default_task_name) {
                default_task.aliases.push(include.namespace.clone());
                default_task.aliases.extend(include.aliases.iter().cloned());
            }
        }

        Ok(())
    }
}

impl<'de> Deserialize<'de> for Tasks {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Deserializing through `serde_yaml_ng::Value` rejects duplicate mapping
        // keys. This is a deliberate divergence from Go, whose decoder silently
        // lets a repeated task name overwrite the earlier one (last wins): a
        // duplicate task definition is almost always a copy-paste mistake, so it
        // is surfaced as an error rather than resolved silently.
        let value = Value::deserialize(deserializer)
            .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
        match value {
            Value::Mapping(map) => {
                let mut tasks = Tasks::new();
                for (k, v) in map {
                    let key = k.as_str().map(str::to_string).ok_or_else(|| {
                        de::Error::custom(TaskfileDecodeError::type_message("tasks"))
                    })?;
                    let mut task: Task = serde_yaml_ng::from_value(v)
                        .map_err(|e| de::Error::custom(TaskfileDecodeError::wrap(e)))?;
                    task.task = key.clone();
                    // The YAML value nodes lose line/column information once
                    // decoded through `serde_yaml_ng::Value`, so the location
                    // is recorded with only the taskfile carried later.
                    task.location = Some(Location::default());
                    tasks.set(key, task);
                }
                Ok(tasks)
            }
            _ => Err(de::Error::custom(TaskfileDecodeError::type_message(
                "tasks",
            ))),
        }
    }
}

/// Prefixes `task_name` with `namespace` and the namespace separator, unless
/// the name already begins with the separator (an absolute reference), in which
/// case the leading separator is stripped and the name used as-is.
fn task_name_with_namespace(task_name: &str, namespace: &str) -> String {
    if let Some(after) = task_name.strip_prefix(NAMESPACE_SEPARATOR) {
        return after.to_string();
    }
    format!("{namespace}{NAMESPACE_SEPARATOR}{task_name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_insertion_order() {
        let input = "build:\n  cmds:\n    - echo build\ntest:\n  cmds:\n    - echo test\n";
        let tasks: Tasks = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(tasks.keys(Sorter::None), vec!["build", "test"]);
        assert_eq!(tasks.get("build").unwrap().task, "build");
    }

    #[test]
    fn sorter_alpha_numeric() {
        let input = "z:\n  cmds: [echo z]\na:\n  cmds: [echo a]\n";
        let tasks: Tasks = serde_yaml_ng::from_str(input).unwrap();
        assert_eq!(tasks.keys(Sorter::AlphaNumeric), vec!["a", "z"]);
    }

    #[test]
    fn merge_applies_namespace() {
        let mut base = Tasks::new();
        let mut other = Tasks::new();
        let task = Task {
            task: "build".to_string(),
            ..Default::default()
        };
        other.set("build".to_string(), task);
        let include = Include {
            namespace: "sub".to_string(),
            ..Default::default()
        };
        base.merge(&other, &include, &Vars::new()).unwrap();
        assert!(base.get("sub:build").is_some());
        assert_eq!(base.get("sub:build").unwrap().namespace, "sub");
    }

    #[test]
    fn merge_flatten_keeps_name() {
        let mut base = Tasks::new();
        let mut other = Tasks::new();
        let task = Task {
            task: "build".to_string(),
            ..Default::default()
        };
        other.set("build".to_string(), task);
        let include = Include {
            namespace: "sub".to_string(),
            flatten: true,
            ..Default::default()
        };
        base.merge(&other, &include, &Vars::new()).unwrap();
        assert!(base.get("build").is_some());
    }

    #[test]
    fn merge_excludes_task() {
        let mut base = Tasks::new();
        let mut other = Tasks::new();
        other.set("build".to_string(), Task::default());
        other.set("test".to_string(), Task::default());
        let include = Include {
            namespace: "sub".to_string(),
            excludes: vec!["build".to_string()],
            ..Default::default()
        };
        base.merge(&other, &include, &Vars::new()).unwrap();
        assert!(base.get("sub:build").is_none());
        assert!(base.get("sub:test").is_some());
    }

    #[test]
    fn merge_default_task_aliased_to_namespace() {
        let mut base = Tasks::new();
        let mut other = Tasks::new();
        other.set("default".to_string(), Task::default());
        let include = Include {
            namespace: "sub".to_string(),
            ..Default::default()
        };
        base.merge(&other, &include, &Vars::new()).unwrap();
        let default_task = base.get("sub:default").unwrap();
        assert!(default_task.aliases.contains(&"sub".to_string()));
    }
}

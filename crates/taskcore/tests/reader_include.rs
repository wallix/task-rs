//! Integration tests for recursive include resolution, merging, and cycle
//! detection in the Taskfile reader.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use std::fs;
use std::path::{Path, PathBuf};

use taskcore::ast;
use taskcore::reader::{FileNode, Reader, ReaderError, new_root_node};

fn scratch(name: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!(
        "taskcore-reader-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, contents).unwrap();
    p
}

#[test]
fn reads_single_taskfile() {
    let d = scratch("single");
    let tf = write(
        &d,
        "Taskfile.yml",
        "version: '3'\ntasks:\n  build:\n    cmds:\n      - echo build\n",
    );

    let node = FileNode::new(&tf.to_string_lossy(), "").unwrap();
    let mut graph = Reader::new().read(&node).unwrap();
    let merged = graph.merge().unwrap();

    assert_eq!(merged.version.as_deref(), Some("3"));
    assert!(merged.tasks.get("build").is_some());
    // The task's location is stamped with the Taskfile path.
    let task = merged.tasks.get("build").unwrap();
    assert!(
        task.location
            .as_ref()
            .map(|l| l.taskfile.ends_with("Taskfile.yml"))
            .unwrap_or(false)
    );

    fs::remove_dir_all(&d).ok();
}

#[test]
fn resolves_and_merges_namespaced_include() {
    let d = scratch("include");
    write(
        &d,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  lib: ./lib/Taskfile.yml\ntasks:\n  root:\n    cmds:\n      - echo root\n",
    );
    let libdir = d.join("lib");
    fs::create_dir_all(&libdir).unwrap();
    write(
        &libdir,
        "Taskfile.yml",
        "version: '3'\ntasks:\n  hello:\n    cmds:\n      - echo hello\n",
    );

    let node = new_root_node(&d.join("Taskfile.yml").to_string_lossy(), "").unwrap();
    let mut graph = Reader::new().read(node.as_ref()).unwrap();
    let merged = graph.merge().unwrap();

    assert!(merged.tasks.get("root").is_some());
    // The included task is namespaced with the include key.
    let sep = ast::NAMESPACE_SEPARATOR;
    assert!(merged.tasks.get(&format!("lib{sep}hello")).is_some());

    fs::remove_dir_all(&d).ok();
}

#[test]
fn nested_includes_resolve_relative_to_each_file() {
    let d = scratch("nested");
    write(
        &d,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  a: ./a/Taskfile.yml\ntasks:\n  root:\n    cmds: [echo root]\n",
    );
    let adir = d.join("a");
    fs::create_dir_all(&adir).unwrap();
    write(
        &adir,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  b: ./b/Taskfile.yml\ntasks:\n  atask:\n    cmds: [echo a]\n",
    );
    let bdir = adir.join("b");
    fs::create_dir_all(&bdir).unwrap();
    write(
        &bdir,
        "Taskfile.yml",
        "version: '3'\ntasks:\n  btask:\n    cmds: [echo b]\n",
    );

    let node = new_root_node(&d.join("Taskfile.yml").to_string_lossy(), "").unwrap();
    let mut graph = Reader::new().read(node.as_ref()).unwrap();
    let merged = graph.merge().unwrap();

    let sep = ast::NAMESPACE_SEPARATOR;
    assert!(merged.tasks.get("root").is_some());
    assert!(merged.tasks.get(&format!("a{sep}atask")).is_some());
    assert!(merged.tasks.get(&format!("a{sep}b{sep}btask")).is_some());

    fs::remove_dir_all(&d).ok();
}

#[test]
fn optional_missing_include_is_skipped() {
    let d = scratch("optional");
    write(
        &d,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  missing:\n    taskfile: ./nope/Taskfile.yml\n    optional: true\ntasks:\n  root:\n    cmds: [echo root]\n",
    );

    let node = new_root_node(&d.join("Taskfile.yml").to_string_lossy(), "").unwrap();
    let mut graph = Reader::new().read(node.as_ref()).unwrap();
    let merged = graph.merge().unwrap();
    assert!(merged.tasks.get("root").is_some());

    fs::remove_dir_all(&d).ok();
}

#[test]
fn non_optional_missing_include_errors() {
    let d = scratch("missing");
    write(
        &d,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  missing: ./nope/Taskfile.yml\ntasks:\n  root:\n    cmds: [echo root]\n",
    );

    let node = new_root_node(&d.join("Taskfile.yml").to_string_lossy(), "").unwrap();
    let err = Reader::new().read(node.as_ref()).unwrap_err();
    assert!(matches!(err, ReaderError::NotFound(_)));

    fs::remove_dir_all(&d).ok();
}

#[test]
fn include_cycle_is_detected() {
    let d = scratch("cycle");
    write(
        &d,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  b: ./b/Taskfile.yml\ntasks:\n  a:\n    cmds: [echo a]\n",
    );
    let bdir = d.join("b");
    fs::create_dir_all(&bdir).unwrap();
    // b includes the root, forming a cycle.
    write(
        &bdir,
        "Taskfile.yml",
        "version: '3'\nincludes:\n  a: ../Taskfile.yml\ntasks:\n  b:\n    cmds: [echo b]\n",
    );

    let node = new_root_node(&d.join("Taskfile.yml").to_string_lossy(), "").unwrap();
    let err = Reader::new().read(node.as_ref()).unwrap_err();
    assert!(matches!(err, ReaderError::Cycle { .. }));

    fs::remove_dir_all(&d).ok();
}

#[test]
fn missing_version_errors() {
    let d = scratch("noversion");
    let tf = write(&d, "Taskfile.yml", "tasks:\n  a:\n    cmds: [echo a]\n");

    let node = FileNode::new(&tf.to_string_lossy(), "").unwrap();
    let err = Reader::new().read(&node).unwrap_err();
    assert!(matches!(err, ReaderError::MissingVersion { .. }));

    fs::remove_dir_all(&d).ok();
}

#[test]
fn invalid_yaml_reports_snippet() {
    let d = scratch("invalid");
    let tf = write(&d, "Taskfile.yml", "version: '3'\ntasks:\n  - not a map\n");

    let node = FileNode::new(&tf.to_string_lossy(), "").unwrap();
    let err = Reader::new().read(&node).unwrap_err();
    match err {
        ReaderError::Invalid { uri, err } => {
            assert!(!uri.is_empty());
            assert!(!err.is_empty());
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    fs::remove_dir_all(&d).ok();
}

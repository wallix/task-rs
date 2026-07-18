//! Behavioral parity tests ported from Go `task_test.go` (cache and
//! export/import behaviors): file-based caching, cache restore/hit, cache
//! disabled, conditional cache, taskfile-level cache inheritance, checksum
//! consistency, dep-generate collection and cache export/import archives.
//!
//! See `common` for the harness. The Go tests drive the `ExportCache` /
//! `ImportCache` / `Run` executor API directly; here we drive the real binary
//! via the `--export-cache` / `--import-cache` flags and the `CACHE_DIR` /
//! `ENABLE_CACHE` / `GREETING` env vars that the testdata Taskfiles read
//! through `sh:` vars.
//!
//! Checksum *values* differ from Go (xxh3 vs md5), so these tests assert on
//! behavior (cache hit/miss, file existence, internal checksum consistency),
//! never on a hardcoded checksum string.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

mod common;
use common::*;

use std::path::Path;
use std::process::Command;

/// Runs the binary in `dir` with `args` and the given `(key, value)` env vars,
/// capturing stdout+stderr merged in write order (matching the Go tests which
/// point both streams at one buffer).
fn run_env(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> (String, i32) {
    let out = Command::new(BIN)
        .args(args)
        .current_dir(dir)
        .envs(env.iter().copied())
        .output()
        .expect("spawn task binary");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (s, out.status.code().unwrap_or(-1))
}

/// Number of entries in `dir`, or 0 if it doesn't exist.
fn dir_len(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|r| r.count()).unwrap_or(0)
}

/// A fresh, not-yet-created cache directory under a unique temp path.
fn cache_dir() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("task-cachedir-{}-{}", std::process::id(), n))
}

// Writes a minimal Taskfile project (input.txt + Taskfile.yml, no cache) into a
// fresh temp dir. Mirrors the inline projects several Go export/import tests
// build.
fn stage_inline(taskfile: &str, input: &str) -> std::path::PathBuf {
    let dir = cache_dir().with_extension("proj");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("input.txt"), input).unwrap();
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();
    dir
}

const EXPORT_TASKFILE: &str = r#"version: '3'
tasks:
  build:
    sources:
      - input.txt
    generates:
      - output.txt
    cmds:
      - cp input.txt output.txt
"#;

// Ports Go `TestExportImportCache`. Run the task so it becomes up to date,
// export the cache to a zip, delete the generated file and checksum dir, then
// import the cache and confirm the generated file is restored and the task is
// once again up to date.
#[test]
fn export_import_cache() {
    let dir = stage_inline(EXPORT_TASKFILE, "hello");

    assert!(run(&dir, &["build"]).ok());
    assert!(dir.join("output.txt").exists());

    let cache = dir.join("cache.zip");
    let cache_s = cache.to_str().unwrap();
    let exp = run(&dir, &["--export-cache", cache_s, "build"]);
    assert!(exp.ok(), "export failed: {}", exp.combined());
    assert!(cache.exists(), "cache zip should be created");

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let imp = run(&dir, &["--import-cache", cache_s, "build"]);
    assert!(imp.ok(), "import failed: {}", imp.combined());

    assert!(dir.join("output.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("output.txt")).unwrap(),
        "hello"
    );

    // Checksums restored, so the task should now be up to date.
    let again = run(&dir, &["build"]);
    assert!(
        again.combined().contains("up to date"),
        "expected up-to-date after import: {}",
        again.combined()
    );
}

// Ports Go `TestExportCacheSkipsNotUpToDate`. A task that has never run is not
// up to date, so export must produce no zip.
#[test]
fn export_cache_skips_not_up_to_date() {
    let dir = stage_inline(EXPORT_TASKFILE, "hello");
    let cache = dir.join("cache.zip");
    let exp = run(&dir, &["--export-cache", cache.to_str().unwrap(), "build"]);
    assert!(exp.ok(), "export invocation failed: {}", exp.combined());
    assert!(
        !cache.exists(),
        "cache file should not exist when no tasks are up to date"
    );
}

// Ports Go `TestExportCacheUnmodified`. Exporting twice without changes prints
// "exporting cache" the first time and detects the archive is "unmodified" the
// second.
#[test]
fn export_cache_unmodified() {
    let dir = stage_inline(EXPORT_TASKFILE, "hello");
    assert!(run(&dir, &["build"]).ok());

    let cache = dir.join("cache.zip");
    let cache_s = cache.to_str().unwrap();

    let first = run(&dir, &["--export-cache", cache_s, "build"]);
    assert!(
        first.combined().contains("exporting cache"),
        "first export: {}",
        first.combined()
    );

    let second = run(&dir, &["--export-cache", cache_s, "build"]);
    assert!(
        second.combined().contains("unmodified"),
        "second export should be unmodified: {}",
        second.combined()
    );
}

// Ports Go `TestCacheRestoreHit`. First run populates a file:// cache; after
// deleting the generate and checksum dir, a second run restores from cache
// instead of executing the command.
#[test]
fn cache_restore_hit() {
    let dir = stage("cache");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (first, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "first run failed: {first}");
    assert!(dir.join("output.txt").exists());
    assert_eq!(dir_len(&cache), 1, "cache dir should have one zip");

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (second, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "second run failed: {second}");
    assert!(dir.join("output.txt").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("output.txt")).unwrap(),
        "hello\n"
    );
    assert!(
        second.contains("restored from cache"),
        "expected cache restore: {second}"
    );
}

// Ports Go `TestCacheDisabled`. With `cache.enabled: false` the cache dir must
// never be created.
#[test]
fn cache_disabled() {
    let dir = stage("cache_disabled");
    let cache = cache_dir();
    let (out, code) = run_env(&dir, &["build"], &[("CACHE_DIR", cache.to_str().unwrap())]);
    assert_eq!(code, 0, "run failed: {out}");
    assert!(
        !cache.exists(),
        "cache dir should not exist when cache is disabled"
    );
}

// Ports Go `TestCacheDisabledSkipsLocker`. A disabled cache whose lock points
// at a nonexistent redis host must still let the task run successfully — the
// locker must not be evaluated when the cache is disabled.
#[test]
fn cache_disabled_skips_locker() {
    let dir = stage("cache_disabled_with_lock");
    let (out, code) = run_env(&dir, &["build"], &[]);
    assert_eq!(code, 0, "run failed (locker evaluated?): {out}");
    assert!(
        dir.join("output.txt").exists(),
        "output.txt should exist after successful run"
    );
}

// Ports Go `TestCacheEnabledShellCondition`. The testdata condition uses the
// Go template function `ne` (`{{ne .ENABLE_CACHE ""}}`).
#[test]
fn cache_enabled_shell_condition() {
    let dir = stage("cache_conditional");
    let cache = cache_dir();
    // ENABLE_CACHE unset -> condition false -> cache not used.
    let (out, code) = run_env(
        &dir,
        &["build"],
        &[("CACHE_DIR", cache.to_str().unwrap()), ("ENABLE_CACHE", "")],
    );
    assert_eq!(code, 0, "run failed: {out}");
    assert!(
        !cache.exists(),
        "cache should not be used when condition is false"
    );
}

// Ports Go `TestCacheEnabledConditionTrue`. As above, with the condition true.
#[test]
fn cache_enabled_condition_true() {
    let dir = stage("cache_conditional");
    let cache = cache_dir();
    let env = [
        ("CACHE_DIR", cache.to_str().unwrap()),
        ("ENABLE_CACHE", "1"),
    ];
    let (out, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "run failed: {out}");
    assert_eq!(
        dir_len(&cache),
        1,
        "cache should be used when condition true"
    );

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (out2, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "second run failed: {out2}");
    assert!(dir.join("output.txt").exists());
    assert!(out2.contains("restored from cache"), "{out2}");
}

// Ports Go `TestCacheInheritFromTaskfile`. A bare-string `cache: default`
// reference is merged against the taskfile-level `caches:` map.
#[test]
fn cache_inherit_from_taskfile() {
    let dir = stage("cache_inherit");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (out, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "run failed: {out}");
    assert!(dir.join("output.txt").exists());
    assert_eq!(dir_len(&cache), 1, "cache dir should have one zip");

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (out2, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "second run failed: {out2}");
    assert!(dir.join("output.txt").exists());
    assert!(out2.contains("restored from cache"), "{out2}");
}

// Ports Go `TestCacheInheritWithOverride`. The task inherits from a redis://
// taskfile default but overrides `url:` to a file:// location, which must be
// used. The explicit `url:` means no `caches:` merge is required, so this works
// in the Rust port.
#[test]
fn cache_inherit_with_override() {
    let dir = stage("cache_inherit_override");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (out, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "run failed: {out}");
    assert!(dir.join("output.txt").exists());
    assert_eq!(
        dir_len(&cache),
        1,
        "cache dir should have one zip from the overridden file:// url"
    );

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (out2, code) = run_env(&dir, &["build"], &env);
    assert_eq!(code, 0, "second run failed: {out2}");
    assert!(dir.join("output.txt").exists());
    assert!(
        out2.contains("restored from cache"),
        "expected restore from overridden url: {out2}"
    );
}

// Ports Go `TestCacheURLSafeTaskName`. The inherited `caches:` model uses
// `{{urlsafe .TASK}}` in its url, exercising the function-position sprig helper.
#[test]
fn cache_url_safe_task_name() {
    let dir = stage("cache_inherit");
    let cache = cache_dir();
    let (out, code) = run_env(&dir, &["build"], &[("CACHE_DIR", cache.to_str().unwrap())]);
    assert_eq!(code, 0, "run failed: {out}");
    assert_eq!(
        dir_len(&cache),
        1,
        "cache should be saved with urlsafe name"
    );
    for entry in std::fs::read_dir(&cache).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(
            !name.contains(':'),
            "cache filename should be URL-safe: {name}"
        );
    }
}

// Ports Go `TestCacheChecksumConsistency`. Asserts behavioral consistency of the
// CHECKSUM template variable: the cache zip filename embeds the same checksum
// that the command writes; the task is up to date on a warm run and restores
// from cache after the output is deleted; changing a command or an input source
// changes the checksum, while changing only a *variable value* does not (raw
// command templates are checksummed) even though the resolved output reflects
// the new value. Checksum *values* differ from Go (xxh3), so all assertions are
// on relationships between checksums, never a hardcoded string.
#[test]
fn cache_checksum_consistency() {
    let dir = stage("cache_checksum_consistency");
    let cache = cache_dir();
    let taskfile = dir.join("Taskfile.yml");
    let input = dir.join("input.txt");
    let output = dir.join("output.txt");

    let read_checksum = || -> String {
        let c = std::fs::read_to_string(&output).unwrap();
        c.split_whitespace().next().unwrap().to_string()
    };
    let cache_zips = || -> Vec<String> {
        std::fs::read_dir(&cache)
            .map(|r| {
                r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    let run_build = |greeting: &str| {
        let (out, code) = run_env(
            &dir,
            &["build"],
            &[
                ("CACHE_DIR", cache.to_str().unwrap()),
                ("GREETING", greeting),
            ],
        );
        assert_eq!(code, 0, "run failed: {out}");
        out
    };

    // First run populates cache and output.txt.
    run_build("hello");
    let orig = read_checksum();
    assert!(!orig.is_empty());

    // Cache zip filename must embed the same checksum written to output.txt.
    let zips = cache_zips();
    assert_eq!(zips.len(), 1);
    assert_eq!(
        zips[0],
        format!("build-{orig}.zip"),
        "cache zip filename should contain the CHECKSUM used in the command"
    );

    // Warm run is up to date.
    assert!(run_build("hello").contains("up to date"));

    // Delete output + checksum -> restore from cache with the same checksum.
    std::fs::remove_file(&output).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();
    assert!(run_build("hello").contains("restored from cache"));
    assert_eq!(orig, read_checksum(), "restored output keeps same CHECKSUM");

    // Changing the command changes the checksum.
    let taskfile_bytes = std::fs::read_to_string(&taskfile).unwrap();
    let modified = taskfile_bytes.replacen(
        r#"echo "{{.CHECKSUM}} {{.GREETING}}" > output.txt"#,
        r#"printf "{{.CHECKSUM}} {{.GREETING}}" > output.txt"#,
        1,
    );
    assert_ne!(modified, taskfile_bytes, "sed replacement must apply");
    std::fs::write(&taskfile, &modified).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();
    run_build("hello");
    let cmd_changed = read_checksum();
    assert_ne!(orig, cmd_changed, "changing a command changes CHECKSUM");

    // Restore command, change input -> checksum differs from both.
    std::fs::write(&taskfile, &taskfile_bytes).unwrap();
    std::fs::write(&input, "changed\n").unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();
    run_build("hello");
    let input_changed = read_checksum();
    assert_ne!(orig, input_changed, "changing input changes CHECKSUM");
    assert_ne!(cmd_changed, input_changed);

    // Restore original input -> checksum matches the original.
    std::fs::write(&input, "hello\n").unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();
    run_build("hello");
    assert_eq!(orig, read_checksum(), "restoring inputs restores CHECKSUM");

    // Changing only a variable value does NOT change the checksum, but the
    // resolved output does reflect the new value.
    std::fs::remove_file(&output).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();
    let _ = std::fs::remove_dir_all(&cache);
    run_build("world");
    assert_eq!(
        orig,
        read_checksum(),
        "variable change must not change CHECKSUM"
    );
    let new_output = std::fs::read_to_string(&output).unwrap();
    assert!(
        new_output.contains("world"),
        "resolved output: {new_output}"
    );
    assert!(
        !new_output.contains("hello"),
        "resolved output: {new_output}"
    );
}

// Ports Go `TestCacheMetaSourcesMatchesChecksum`. The `sources:` hash stored in
// the cache zip's comment must equal the CHECKSUM embedded in the zip filename.
#[test]
fn cache_meta_sources_matches_checksum() {
    let dir = stage("cache_checksum_consistency");
    let cache = cache_dir();
    let (out, code) = run_env(
        &dir,
        &["build"],
        &[
            ("CACHE_DIR", cache.to_str().unwrap()),
            ("GREETING", "hello"),
        ],
    );
    assert_eq!(code, 0, "run failed: {out}");

    let entries: Vec<_> = std::fs::read_dir(&cache).unwrap().collect();
    assert_eq!(entries.len(), 1);
    let zip_name = entries[0].as_ref().unwrap().file_name();
    let zip_name = zip_name.to_string_lossy();
    let checksum = zip_name
        .strip_prefix("build-")
        .and_then(|s| s.strip_suffix(".zip"))
        .expect("zip name shape");
    assert!(!checksum.is_empty());

    let bytes = std::fs::read(cache.join(&*zip_name)).unwrap();
    let comment = read_zip_comment(&bytes);
    let sources_hash = comment
        .lines()
        .find_map(|l| l.strip_prefix("sources:"))
        .expect("sources: line in zip comment");
    assert_eq!(
        checksum, sources_hash,
        "sources hash in zip comment must match CHECKSUM in filename"
    );
}

// Extracts the ZIP archive comment from raw bytes by locating the End Of
// Central Directory record (signature PK\x05\x06). The comment length is the
// last two bytes of the fixed EOCD header and the comment follows it.
fn read_zip_comment(bytes: &[u8]) -> String {
    const SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
    // EOCD is 22 bytes + comment; search from the end for the signature.
    let start = bytes.len().saturating_sub(22 + 0xffff);
    let mut eocd = None;
    for i in (start..=bytes.len().saturating_sub(22)).rev() {
        if bytes[i..i + 4] == SIG {
            eocd = Some(i);
            break;
        }
    }
    let i = eocd.expect("EOCD record");
    let len = u16::from_le_bytes([bytes[i + 20], bytes[i + 21]]) as usize;
    let cstart = i + 22;
    String::from_utf8_lossy(&bytes[cstart..cstart + len]).into_owned()
}

// Ports Go `TestCacheRejectsGeneratesOutsideRootDir`. A cached task whose
// generates escape the project root is rejected at compile time.
#[test]
fn cache_rejects_generates_outside_root_dir() {
    let dir = cache_dir().with_extension("proj");
    let outside = cache_dir().with_extension("outside");
    let cache = cache_dir();
    for d in [&dir, &outside, &cache] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(dir.join("input.txt"), "hello").unwrap();
    std::fs::write(outside.join("escaped.txt"), "escaped").unwrap();
    let taskfile = format!(
        r#"version: '3'
tasks:
  build:
    sources:
      - input.txt
    generates:
      - "{outside}/escaped.txt"
    cache:
      url: "file://{cache}/build-{{{{.CHECKSUM}}}}.zip"
    cmds:
      - echo ok
"#,
        outside = outside.display(),
        cache = cache.display(),
    );
    std::fs::write(dir.join("Taskfile.yml"), taskfile).unwrap();

    let (out, code) = run_env(&dir, &["build"], &[]);
    assert_ne!(
        code, 0,
        "run should fail for generates outside project root"
    );
    assert!(out.contains("outside project root"), "error text: {out}");
}

// Ports Go `TestCacheDepGenerates`. A wrapper task with cache enabled but no
// generates of its own uses `from: deps`; the dep's generates are folded into
// the wrapper's cache archive.
#[test]
fn cache_dep_generates() {
    let dir = stage("cache_dep_generates");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (out, code) = run_env(&dir, &["wrapper"], &env);
    assert_eq!(code, 0, "run failed: {out}");
    assert!(dir.join("output-a.txt").exists());
    assert!(dir.join("output-b.txt").exists());
    assert_eq!(dir_len(&cache), 1, "cache dir should have one zip");

    std::fs::remove_file(dir.join("output-a.txt")).unwrap();
    std::fs::remove_file(dir.join("output-b.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (out2, code) = run_env(&dir, &["wrapper"], &env);
    assert_eq!(code, 0, "second run failed: {out2}");
    assert_eq!(
        std::fs::read_to_string(dir.join("output-a.txt")).unwrap(),
        "hello\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("output-b.txt")).unwrap(),
        "hello\n"
    );
    assert!(out2.contains("restored from cache"), "{out2}");
}

// Ports Go `TestCacheDepGeneratesNested`. Like `cache_dep_generates`, one
// dependency level deeper (`from: deps`/`from: cmds` folded transitively).
#[test]
fn cache_dep_generates_nested() {
    let dir = stage("cache_dep_generates_nested");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (out, code) = run_env(&dir, &["top"], &env);
    assert_eq!(code, 0, "run failed: {out}");
    assert!(dir.join("output.txt").exists());
    assert_eq!(dir_len(&cache), 1, "cache dir should have one zip");

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (out2, code) = run_env(&dir, &["top"], &env);
    assert_eq!(code, 0, "second run failed: {out2}");
    assert_eq!(
        std::fs::read_to_string(dir.join("output.txt")).unwrap(),
        "hello\n"
    );
    assert!(out2.contains("restored from cache"), "{out2}");
}

// Ports Go `TestCacheHitSkipsDeps`. A wrapper whose cache generates come from
// `from: deps` is saved and restored, so a cache hit skips running the deps.
#[test]
fn cache_hit_skips_deps() {
    let dir = stage("cache_skips_deps");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    let (first, code) = run_env(&dir, &["wrapper"], &env);
    assert_eq!(code, 0, "first run failed: {first}");
    assert!(dir.join("output.txt").exists());
    assert!(
        first.contains("EXPENSIVE-BUILD-RAN"),
        "dep should run first time: {first}"
    );
    assert_eq!(dir_len(&cache), 1, "cache dir should have one zip");

    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (second, code) = run_env(&dir, &["wrapper"], &env);
    assert_eq!(code, 0, "second run failed: {second}");
    assert!(dir.join("output.txt").exists());
    assert!(second.contains("restored from cache"), "{second}");
    assert!(
        !second.contains("EXPENSIVE-BUILD-RAN"),
        "deps should NOT run on cache hit: {second}"
    );
}

// Ports Go `TestCachedDepSkipsExpensiveWork`. A cache-less wrapper depends on a
// build task that has concrete generates and its own file:// cache. A first
// invocation runs the expensive build and populates the cache; a fresh
// invocation (no local state, only the shared cache) restores the build dep
// from cache without re-running the expensive work. The dep has concrete
// generates, so this works in the Rust port.
#[test]
fn cached_dep_skips_expensive_work() {
    let taskfile = r#"version: '3'
vars:
  CACHE_DIR:
    sh: echo "$CACHE_DIR"

tasks:
  manuals:
    deps:
      - manuals:build

  manuals:build:
    sources:
      - input.txt
    generates:
      - output.txt
    cache:
      url: 'file://{{.CACHE_DIR}}/build-{{.CHECKSUM}}.zip'
    cmds:
      - echo "BUILD-RAN" >&2
      - cp input.txt output.txt
"#;
    let dir = stage_inline(taskfile, "hello\n");
    let cache = cache_dir();
    let env = [("CACHE_DIR", cache.to_str().unwrap())];

    // Job A: build + populate cache.
    let (first, code) = run_env(&dir, &["manuals"], &env);
    assert_eq!(code, 0, "job A failed: {first}");
    assert!(first.contains("BUILD-RAN"), "build dep should run: {first}");
    assert!(dir.join("output.txt").exists());

    // Job B: fresh state, only the shared cache.
    std::fs::remove_file(dir.join("output.txt")).unwrap();
    std::fs::remove_dir_all(dir.join(".task")).unwrap();

    let (second, code) = run_env(&dir, &["manuals"], &env);
    assert_eq!(code, 0, "job B failed: {second}");
    assert!(dir.join("output.txt").exists());
    assert!(
        second.contains("restored from cache"),
        "wrapper should restore build dep from cache: {second}"
    );
    assert!(
        !second.contains("BUILD-RAN"),
        "build dep should NOT run on cache hit: {second}"
    );
}

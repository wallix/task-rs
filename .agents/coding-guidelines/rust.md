# Rust Coding Guidelines

Applies to: every Rust crate in the workspace (`taskcore`, `task`, `ocicas`).

See [`../coding-guidelines.md`](../coding-guidelines.md) for general conventions
and formatting requirements that apply to all code.

## Conventions

- Single responsibility per module; avoid cyclic dependencies. Logic belongs in
  `taskcore`; the `task` crate stays a thin CLI over it, and `ocicas` stays free
  of task-specific concerns (it is meant to be shared back with virtkit).
- When you change a function's signature or return type, update every call site.
  A successful build does not prove all callers are covered — use repo-wide
  search or find-references first.
- `Result` with custom error enums; validate external inputs early; no
  panics/unwraps on untrusted data. Use inline interpolation in format macros:
  `write!(f, "Error: {key}")` not `write!(f, "Error: {}", key)`.
- **Panics are a bug, and the lints enforce it:** the workspace denies
  `unwrap_used`, `expect_used`, `panic`, `indexing_slicing` and
  `arithmetic_side_effects` (`Cargo.toml`, `[workspace.lints.clippy]`). Use `?`,
  `.get()`, `checked_*`, `TryFrom`, and surface real errors. Relax a lint at the
  narrowest scope that works, never by loosening the workspace block. The
  pattern in the tree is a crate-root `#![cfg_attr(test,
  allow(clippy::unwrap_used, ...))]` so unit tests may panic while library code
  may not (`crates/*/src/lib.rs`, `crates/task/src/main.rs`), plus a file-level
  `#![allow(...)]` at the top of each integration-test file. Note that
  `Cargo.toml`'s own comment describes this as `#[cfg_attr(test, allow(...))]`
  "at the call sites" — no call site does that; the crate-root form is what is
  actually used.
- **Propagate errors:** do not discard `Result` with `.ok()`,
  `.unwrap_or_default()`, or `let _ =` without a comment explaining why the
  failure is safe to ignore. Exit codes are part of the compatibility contract:
  propagate the worst exit code, not just the last one.
- **TOCTOU on paths:** never trust a `&Path` across two syscalls. Anchor
  operations on a file descriptor (`openat`, `fstatat`, `*at` family) instead of
  re-resolving the path. Use `OpenOptions::create_new(true)` when creating files
  to reject symlinks. If you act on the same path twice, assume it is a TOCTOU
  bug until proven otherwise. This matters throughout fingerprinting and the
  cache, which stat and then read the same files.
- **Confine every extraction:** cache archives (zip, and the `ocicas` chunk
  store) come from a remote registry and are untrusted. Reject absolute paths,
  `..` components, and symlinks that escape the destination *before* writing —
  resolve the joined path and verify it is still under the destination root. A
  "trusted" registry is not an argument.
- **Permissions at creation:** set file/directory permissions at creation time
  with `OpenOptions::mode()` / `DirBuilderExt::mode()`, not with a separate
  `fs::set_permissions` call after creation (race window).
- **Path identity:** never compare paths as strings. Use `fs::canonicalize` or
  compare `(dev, inode)` pairs for filesystem identity.
- **Stay in bytes at Unix boundaries:** use `Path`/`PathBuf` for filesystem
  paths, `OsString`/`OsStr` for env vars, and `&[u8]`/`Vec<u8>` for stream
  contents and command output. Never round-trip through `String`; avoid
  `from_utf8_lossy` (silent data corruption) and `from_utf8().unwrap()` (panic
  on valid Unix input). Prefer `Write::write_all` over `print!`/`format!` for
  binary data.
- **Determinism in the cache path:** fingerprint and task-identity hashes must
  depend only on declared inputs — no iteration over a `HashMap`, no ambient
  environment, no timestamps, no absolute host paths. Use ordered collections
  (`BTreeMap`, `IndexMap`) or sort before hashing. The checksums are
  byte-compatible with the Go implementation; changing what goes into one
  invalidates every existing `.task` cache.
- **Concurrency:** the engine runs tasks in parallel. Anything reached from a
  task body must be safe under concurrent execution, and shared state that
  guards "run once" semantics (`run: once`, setup tasks, distributed locks) must
  be correct under contention, not just under a single-threaded test.
- Deterministic seeds for property/fuzz tests. Co-locate fast unit tests in the
  module they cover; behavioural parity tests go in `crates/task/tests/` driving
  the real binary against a `testdata/<case>` fixture, and heavier library
  integration tests under `crates/taskcore/tests/`.
- Measure before optimizing; document benchmark context when micro-optimizing.

## Dependencies — favor the standard library

See [General Coding
Conventions](../coding-guidelines.md#general-coding-conventions) for the
rationale. Concrete guidance:

- Dependency versions are centralized in the workspace `Cargo.toml`
  (`[workspace.dependencies]`), and **every entry beyond the foundational few
  (`serde`, `serde_json`, `tokio`, `anyhow`) carries a comment saying why it
  earns its place**. Adding one without that rationale is incomplete; members
  reference them with `<dep>.workspace = true` and add only the per-crate
  features they need. The `brush-*` interpreter crates are the current exception
  to the centralization — they are pinned directly in
  `crates/taskcore/Cargo.toml`.
- `std::collections` (`HashMap`, `BTreeMap`, `HashSet`) over
  `hashbrown`/`dashmap`. `indexmap` is already in and is the right answer where
  the Taskfile schema needs insertion order (vars, tasks) — not as a
  general-purpose map.
- `std::sync` (`Mutex`, `RwLock`, `Arc`) over `parking_lot` unless contention is
  measured.
- `thiserror` only when an error enum has many variants and the boilerplate
  genuinely hurts. Neither `thiserror` nor `anyhow` is used today: every crate
  hand-rolls its error enum, and the `task` binary maps it to an exit code
  (`crates/task/src/main.rs`). `anyhow` is declared in
  `[workspace.dependencies]` but unused — if it ever lands, only at application
  boundaries (the `task` binary, top-level handlers), never in `taskcore` or
  `ocicas` APIs. For small error types, hand-written `Display`/`From` impls are
  fine.
- `std::process::Command` over `duct`/`subprocess`. Shell command bodies go
  through the `brush` interpreter, not by shelling out to `/bin/sh`.
- `serde` + `serde_json` / `serde_yaml_ng` for structured I/O — but resist a
  derive crate just for one struct; a 5-line `Display` impl is sometimes the
  right answer.
- Acceptable where stdlib is genuinely insufficient, and already present:
  `serde`, `tokio` (no async runtime in stdlib; used by the registry/cache
  paths), `regex`, `semver`, `notify`, `minijinja`, `clap`, `redis` (the
  distributed `cache.lock` backend), the `brush-*` shell interpreter, and the
  hashing/compression/registry stack (`sha2`, `twox-hash`, `zstd`, `fastcdc`,
  `zip`, `oci-client`, `reqwest`, `rustls`, `jsonwebtoken`). Check
  `[workspace.dependencies]` before concluding a crate is new.
- One crypto provider only: `reqwest` is pinned `rustls-no-provider` and the
  `task` binary installs the ring provider at startup. Do not pull in a dep that
  links a second one.
- New dependencies enlarge the `cargo-audit` surface and must stay statically
  linkable under musl with no system C libraries. Add an advisory ignore only
  with documented rationale and residual risk in `.cargo/audit.toml`.
- Keep the pins shared with virtkit (`oci-client`, `reqwest`, `rustls`,
  `jsonwebtoken`, `zstd`, `sha2`, `fastcdc`, `serde_yaml_ng`) in step with that
  repo — `ocicas` is meant to merge back.

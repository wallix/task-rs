# Changelog

## v4.0.0

Task is now a Rust program. v4.0.0 is a full rewrite that aims to be drop-in
compatible.

### Highlights

- **Local deduplicated cache.** A content-defined-chunking, zstd-compressed,
  content-addressed cache (the `ocicas` crate) backs `--export-cache` /
  `--import-cache` and the OCI cache backend, so shared build outputs are stored
  and transferred once.
- **Native Jinja templating (opt-in), with migration.** Alongside the existing
  Go `text/template` syntax, a Taskfile can opt into native
  [minijinja](https://github.com/mitsuhiko/minijinja) with `templater: jinja`,
  unlocking `{% for %}`, `{% if %}`, filters, and function-call syntax. The
  dialect is auto-detected per file; `task --migrate` converts a Go-syntax
  Taskfile to Jinja (preview by default, `--write` to apply).
- **Go template syntax is deprecated.** Files that still use it get a one-time
  warning pointing at `task --migrate`. Go rendering will be removed in a future
  release; migration will remain. Suppress the warning with
  `TASK_NO_GO_DEPRECATION=1`.
- **Single static binary.** The release build links no system C libraries
  (musl-static, rustls+ring) and ships as one file.

### Intentional differences from Task v3

- A **duplicate task key** in a Taskfile is now an error instead of silently
  taking the last definition.
- **Task env/vars take precedence over the process environment by default.** A
  task-defined `env`/`vars` value overrides one already present in the inherited
  environment; set `TASK_X_ENV_PRECEDENCE=0` to restore the old order where the
  process environment wins.
- **Remote (HTTP) Taskfiles** are not supported (already removed in this fork).

### Known gaps

The following are not yet ported and are planned for follow-up releases:

- Storing cache *contents* in Redis (`cache.url: redis://`) is not supported.
  (The file and OCI cache backends, and the Redis distributed **lock**
  `cache.lock: redis://`, all work.)
- Syntax highlighting in error snippets.

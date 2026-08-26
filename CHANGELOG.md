# Changelog

## Unreleased

## v4.1.0 - 2026-08-26

- **Quoted strings in Go-syntax templates keep their dots**, so
  `{{ .FILE | replace ".tar.gz" "" }}` strips what it was given. Affected tasks
  re-run once, and a Taskfile already converted by `task --migrate --write`
  needs checking by hand.
- **An escaped quote works inside a Go-syntax template string.** A Taskfile
  already converted by `task --migrate --write` needs checking by hand.
- **Go-syntax templates can call the string helpers directly**, not only after
  a pipe, so `{{trimSuffix ".po" .ITEM}}` works. `splitList` was splitting the
  wrong argument. Affected tasks re-run once.
- **`default`, `title`, `join`, `first` and `last` mean the same on either side
  of a pipe** in Go-syntax Taskfiles — most visibly, `default` substitutes for
  any empty value, not only an unset one. Jinja Taskfiles keep Jinja's meaning.
  Affected tasks re-run once, and a Taskfile already converted needs checking by
  hand.
- **The Go-template deprecation warning no longer runs into the next line.**
- **A dependency cycle is reported as one**, naming the path (`a -> b -> a`),
  instead of running until the process ran out of stack. A task that reaches
  itself is rejected: calling itself with different `vars:` still works, but
  recursion driven by state outside the Taskfile no longer does.
- **Deep dependency trees no longer crash the runner.** Thousands of levels
  run; a runaway recursion stops with an error instead.
- **Tasks start in a different order**, so interleaved output can differ.
  `--output group` and `prefixed` still keep each task's output together.
- **A failing dependency no longer stops its siblings starting.** Under
  `--failfast` they all start and are cancelled when one fails, so a cancelled
  sibling may leave partial work behind.
- **A failing run stops the commands it leaves behind** instead of orphaning
  them. It cannot tell those from a job a task backgrounded on purpose, so both
  are stopped; `TASK_NO_REAP=1` turns that off.
- **Ctrl-C reaches the commands, not just the runner.** The second press is
  passed on to them, and the third stops them before exiting.
- **A confirmation prompt no longer pauses the rest of the run**, and Ctrl-C
  works while one is waiting.
- **A `SIGTERM` stops the run at once**, exiting `1` — it comes from a
  supervisor, so there is no second signal to wait for. Ctrl-C keeps its
  three-press escalation, and watch sessions are unchanged.

## v4.0.0 - 2026-08-24

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
- **vk-registry distributed lock.** `cache.lock: vk://host/<prefix>` takes the
  build-once lock over the registry's own `/lock` API, so one vk-registry serves
  both the `oci://` cache and the lock with no separate Redis. Credentials come
  from the URL (Basic) or `$TASK_VK_LOCK_TOKEN` (bearer); the lease is renewed
  by a heartbeat and expires 30 s after a holder goes away.
- **Single static binary.** The release build links no system C libraries
  (musl-static, rustls+ring) and ships as one file.

### Intentional differences from Task v3

- A **duplicate task key** in a Taskfile is now an error instead of silently
  taking the last definition.
- **Task env/vars take precedence over the process environment by default.** A
  task-defined `env`/`vars` value overrides one already present in the inherited
  environment; set `TASK_X_ENV_PRECEDENCE=0` to restore the old order where the
  process environment wins.
- **Remote (HTTP) Taskfiles** are not supported.

### Known gaps

- Storing cache *contents* in Redis (`cache.url: redis://`) is not supported.
  (The file and OCI cache backends, and the Redis and vk-registry distributed
  **locks** — `cache.lock: redis://` and `vk://` — all work.)
- Syntax highlighting in error snippets.

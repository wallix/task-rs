# Changelog

## Unreleased

## v4.3.0 - 2026-08-28

- **Linux release binaries are reproducible** — each release includes a
  `task-linux-<arch>.build-info.txt` manifest for verifying a local rebuild. See
  [reproducible builds](https://github.com/wallix/task-rs/blob/main/docs/reference/reproducible-builds.md).

## v4.2.0 - 2026-08-27

- **`default` is a filter, not a backwards-looking call** — `--migrate` now
  converts both Go spellings, `{{ .X | default "y" }}` and
  `{{ default "y" .X }}`, to `{{ X | default("y", true) }}`. That second
  argument makes Jinja's own filter mean what sprig's `default` means —
  substituting for any empty value, not only an unset one — so a migrated
  Taskfile renders what it always did while reading in the natural order. A Go
  `{{ default "y" }}` with no value at all now renders `y`, as sprig does,
  instead of failing.
- **Breaking:** the sprig-ordered `default(fallback, value)` function is gone.
  A Taskfile written or migrated against 4.1.0 / 4.1.1 that calls it now fails
  with `unknown function`; write the value first and add `, true` —
  `X | default("y", true)`.
- **`trunc` and `regexReplaceAll` work as filters** — `s | trunc(n)` and
  `s | regexReplaceAll(pattern, repl)`, so a Jinja Taskfile no longer has to
  spell either as a sprig-ordered call. The functions keep working as before,
  and a Go Taskfile's `{{ .P | trunc 3 }}`, which used to fail to render, now
  works and migrates to `{{ P | trunc(3) }}`.
- **A file's template dialect now covers the `vars:` it passes to an
  `includes:` entry and the `caches:` models it defines.** A tree partway
  through `--migrate` works: the migrated file no longer fails on its own
  include vars, and a not-yet-migrated task no longer fails on the cache URL it
  inherits. A file that declares one dialect but writes those in the other used
  to be read as Go regardless, and now errors — run `task --migrate` on it.

## v4.1.1 - 2026-08-27

- **`task --update` replaces the binary with a published release** — the latest,
  or `--update=<version>` for a specific one. It asks before touching anything
  (`--yes` skips that), checks the download against the `sha256` published
  beside it, and runs the new binary to confirm it works here before putting it
  in place.
  `task --update --check` only reports what is available, exiting `1` when a
  newer release exists. A task named `update` still runs as it always did.
- **`install-task.sh` installs again** — every run used to fail before it
  downloaded the archive.
- **A failing cache registry or cache lock names the reason** — a rejected
  certificate, a refused connection, a name that does not resolve — instead of
  only reporting that the registry could not be reached.
- **A `vks://` lock trusts the same certificate as the cache**, so one registry
  behind a private CA serves both the cache and the build-once lock. Such a lock
  could not connect before, and every run took a local lock instead.
- **Go-syntax Taskfiles can call `printf` and `print`**, so a Taskfile that
  builds a path out of several variables renders and migrates instead of being
  rejected as an unsupported construct. `printf` covers the string-composing
  verbs (`%s`, `%v`, `%q`, `%d`, `%%`, with flags and a width); another verb, a
  precision, an argument its verb cannot render, or a mismatched argument count
  is an error.
- **An unsupported Go construct is reported as the `{{ … }}` that used it** —
  with the line it is on when migrating a whole Taskfile — instead of quoting
  the string it came from, which under `--migrate` was the whole file.

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

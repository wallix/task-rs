# Changelog

## Unreleased

- **Sources are hashed at compile time only when something reads
  `{{.CHECKSUM}}`.** Compiling a task, the quick compile that checks its
  platforms and required vars, and compiling every task a `task:` or `from:`
  reference reaches no longer hash sources just to fill the variable. A task
  reading it (in commands and their vars, task names, preconditions, the cache
  block, deferred commands or `requires`, through a `ref:` var or Jinja
  `debug()`), and every task under `--summary` or templated `output:` group
  markers, keeps the compile-time value as before. For the others, a task with
  `generates` computes the value once just before its build-once lock, after
  `setup`: the sources hash in the lock name (`<task>:<cache checksum>`), the
  cache key and the cache entry's recorded sources is now taken there rather
  than before `setup`. A task without `generates` takes no lock and has no cache
  entry, and simply reuses its up-to-date reading. A cache entry is now restored
  only when the run's cache key matches what its up-to-date check read, and
  saved only when it matches what the run built from: a key taken before
  preconditions, a lock wait or another writer changed the sources is skipped
  (noted with `-v`), for these tasks and for those keeping the compile-time
  value alike.
- **Fingerprinting walks each directory once and takes file types from the
  listing,** so hashing a regular file costs one `stat` instead of about four.
  A source or generated file removed after its directory was listed now fails
  the task, naming it, where it could be left out before, and so does one
  whose type changed in between (a file replaced by a symlink, say). On
  Windows that check opens reparse points without following them; it is
  type-checked there but not run-tested, and reading files behind
  filter-driver reparse points (deduplicated files, OneDrive placeholders) is
  unverified.
- **A cache restore cancelled before it is verified is undone,** and so is one
  that cannot durably mark itself kept: the task then runs normally, and an
  `--import-cache` fails. Recovering a restore cut short by a crash puts back
  the originals it moved aside over entries it never verified, but only where an
  entry is still exactly what the restore wrote: an entry changed since is kept,
  with a warning naming the original, saved beside it as
  `.task-restore-original-<name>-<random>`. A restore cut short while committing
  keeps its entries.
- **Fingerprints are read off the thread that schedules tasks.** Tasks running
  in parallel now fingerprint at the same time, and interrupts, output and lock
  renewals are handled while a large tree is being hashed.
- **A run reads its sources and generates once after its commands.** The
  drift check, the saved fingerprint and the cache entry all use that one
  reading, instead of reading the files up to three more times. The drift
  warning now names the sources whose content changed, rather than those whose
  size or modification time did.
- **Fingerprinting digests files in parallel,** on at most as many threads as
  there are CPUs available, shared by every task hashing at the same time. The
  checksum does not depend on the number of threads.
- **Fingerprints use a new checksum format.** Each file is recorded with its
  path relative to the task directory, its kind (file or symlink) and its own
  digest, in path order, and the task's rules by their fields, so adjacent
  inputs no longer run together, and sources outside the task directory no
  longer make the checksum depend on the checkout directory's name. Checksums
  are 32 hex digits and no longer match the Go implementation: every task with
  `sources` rebuilds and misses the cache once, and one that renders
  `{{.CHECKSUM}}` in a field its identity covers (commands, their vars, task
  names in `deps`/`setup` and their vars, preconditions, the cache block) also
  gets a new checksum file name. A source that is neither a regular file nor a
  symlink (a named pipe, a device) now fails the task instead of being read, and
  fingerprinting errors name the file.

## v4.8.1 - 2026-10-16

- **`sandbox.writes: [{task: X}]` now grants what X and the tasks it runs
  write:** their `generates` and `sandbox.writes` directories, and reading their
  dotenv files, so a sandboxed task running X, with `task:` or as a nested
  `task X`, needs no other entry for them. Their `sandbox.reads` are not
  granted.
- **A `task:` entry in `sources` now covers what the task runs.** `{task: X}`
  also takes in the sources of the tasks X runs through `setup`, `deps` and
  `task:` commands (including `for:` loops and deferred calls), transitively,
  with the vars those calls pass, so a task running X no longer lists X's
  dependencies. In `generates` it still copies X's own entries, and `from:`
  still copies its direct targets'. Such tasks, and wrappers reaching them, get
  new fingerprints and cache keys, and rebuild once. Calls behind an `if:`
  count too, so a missing task there fails with exit code 200, naming the
  calls leading to it; a deferred call depending on `EXIT_CODE`, or on
  `{{.CHECKSUM}}` in a task with `from:` or `task:` entries of its own, must
  be listed explicitly; a recursion whose end the references cannot see stops
  at 256 calls of one task, or 1000 calls in all, with exit code 204. A cycle
  between references is no longer an error.
- **`from: cmds` now reads templated deferred calls,** as they run after a
  success, instead of looking up their unrendered name; one depending on
  `EXIT_CODE`, or on `{{.CHECKSUM}}` in a task with `from:` or `task:` entries
  of its own, is refused. `--watch` follows them the same way.
- **A folded task's `exclude:` entries no longer remove other tasks' files.**
  In `sources` and `generates`, the exclusions of a task folded in by `from:`
  or `task:` apply to that task's own entries only; the referring task's own
  exclusions still apply to everything it folds in. Tasks folding a task that
  excludes files get new fingerprints and cache keys, and rebuild once.
- **`{{.CHECKSUM}}` now covers the entries a `from:` or `task:` reference
  folds in** wherever it is rendered after them: deferred commands,
  `preconditions` and the cache block, so a cache key now changes when only a
  referenced task's source does. Tasks with such entries get new cache keys,
  and miss the cache once; their fingerprints change too where the cache block
  embeds `{{.CHECKSUM}}`.
- **A Jinja task with `from:` or `task:` entries renders its `preconditions` as
  Jinja again,** instead of as Go templates.
- **A cache entry now restores only the task's generated files.** A poisoned
  `file://`, `oci://` or `vk` entry could write any file in the project (the
  Taskfile, git hooks, other tasks' checksums) and reach outside it: a zip one
  in 4.8.0 and earlier by its names, either through a hard link already in the
  project (an `oci://` one rewriting the linked file, a zip one changing its
  permissions). An entry holding anything outside the task's `generates`, or cached
  for another task or other sources, is now refused before anything is
  written, and the task runs instead. `--import-cache` still restores every
  task's files, but no restore writes into `.git`, or an import into the temp
  directory's `tmp`. Entries named with a `..` component are refused, and
  restored files and directories lose their setuid, setgid and sticky bits.
- **A cache entry holding a symlink out of the project is refused,** unless
  the task sets `cache: {outside_links: true}` (for a virtualenv, say): the
  task's commands would otherwise write through a link a poisoned entry
  planted in its outputs. Links are followed through those already in the
  project, and one leading to a Taskfile or dotenv file Task read, `.taskrc`,
  `.git` (at any depth: a directory holding a nested repository too) or Task's
  temp directory is refused whatever the setting, so a task whose pnpm-style
  link points at a package holding one of these misses the cache. A task whose
  outputs hold such a link is not saved. An entry naming one file twice is
  refused. An entry whose restored files fail verification is removed again
  before the task runs, the files it replaced put back, and a restore that
  fails partway is undone the same way; if undoing fails, the task fails
  instead of running among restored files. A restore that is killed leaves
  nothing behind once the next restore runs. On Windows no symlink is restored; an
  entry holding one is a cache miss. These checks do not guard against other
  processes changing the project during a restore.

## v4.8.0 - 2026-10-16

- **`sandbox.writes` can name a task.** A `{task: <name>}` entry, with
  optional `vars`, makes the directories that task's `generates` write in
  writable, so a sandboxed task can call it, with `task:` or as a nested
  `task` process, on a fresh checkout. Unlike the task's own `generates`, such
  a directory that also holds sources is not warned about, as only the named
  task can move its outputs. The entry changes neither the task's fingerprint
  nor its cache key; an unknown name fails with exit code 200.
- **A sandboxed task's `generates` outside the root directory no longer fail
  its setup.** One under an existing directory such as `/opt` is left to the
  commands instead of being created beforehand.
- **Tasks can check that their sources are complete (Linux only).** With
  `sandbox: true`, a task's commands and the tasks they call can read only the
  task's sources inside the root directory (the Taskfile's). A file missing
  from `sources` then fails the build instead of leaving a stale up-to-date
  result or cache entry. `sandbox.reads` and `sandbox.writes` widen it. The
  restriction uses Landlock, or a slower seccomp supervisor on kernels without
  it (which also works without `process_vm_readv`, as in some microVM guests);
  with neither available, the default `TASK_SANDBOX=auto` runs commands
  unrestricted with a warning (`TASK_SANDBOX=landlock` or `seccomp` makes it an
  error). Any denial the supervisor sees fails the task, even one the command
  recovered from or ignored, and an enforced sandbox runs a command's `if` under
  the supervisor (without the supervisor, unsandboxed with a warning).
  `TASK_SANDBOX=audit` lists every undeclared access without failing.
- **`sources` and `generates` can copy another task's entries.** A
  `- task: <name>` entry, with optional `vars`, folds in that task's `sources`
  (or `generates`) without it being a dependency or running, so a test or lint
  task stays in step with the build task whose inputs it reads, including
  under `sandbox`. An unknown task name in a `task:` or `from:` entry now fails
  the run or `--status` with exit code 200, naming the referring task and
  field.
- **A Taskfile error with a position now shows the offending lines,** with a
  caret under the error, after the message. Unlike Go Task, YAML syntax errors
  get one too; a type mismatch found while decoding still has no position, so
  no snippet.
- **Taskfiles are parsed with a memory-safe YAML parser.** Checksums and the
  positions of decode errors are unchanged; YAML syntax errors are worded and
  sometimes positioned differently (an unclosed `[`, `{` or quote is reported
  where it opens), and a Taskfile starting with a byte order mark now loads.
  Unlike Go Task, a comment glued to the token before it (`'x'#c`, `|#c`,
  `{b: 1}#c`, `[1,#c`) and `[-]` are refused, `[?x]` reads as the string `?x`,
  `&a#c` names the anchor `a#c`, U+0085, U+2028 and U+2029 are not line
  breaks, and a few inputs Go Task refuses (a tab-indented top-level key, an
  empty key, a stream of only `...`, `%YAML 1.3`) are accepted.

## v4.7.1 - 2026-09-21

- **A task that overwrites its own sources is now called out.** When a task's
  commands write files its `sources:` glob also matches, the checksum changes
  mid-run, so the task can never be cached. Task now prints a warning naming
  those files and skips publishing the task's fingerprint and cache entry;
  previously this was logged only under `--verbose`.
- **Cache save and push failures are now reported.** A cache entry that cannot
  be archived, written, or pushed prints a warning instead of being logged only
  under `--verbose`. An unreachable registry warns once per host; other failures
  warn per task.

## v4.7.0 - 2026-09-21

- **Included tasks share saved fingerprints when their build inputs match,**
  even across separate runs without a build cache. Variables that change
  commands or declared environment values keep separate entries; namespaced
  task references still distinguish copies. Checksum values keep the Go fork's
  format, but filenames differ, so upgrading rebuilds each task once.

## v4.6.2 - 2026-09-19

- **A cache transfer no longer fails on one dropped connection to the registry.**
  A pull or push request whose connection could not be made, timed out, or was
  cut short under the response is now made again a few times, seconds apart,
  before the cache entry is given up on. A registry answer — a miss, a refusal,
  a digest that does not match — is still final at once.

## v4.6.1 - 2026-09-19

- **Cache archives carry the whole generated tree.** Collecting a `generates`
  glob for the cache now includes hidden entries — `node_modules/.bin` and
  every other dot file or directory under `**/*` — and stores a symlink to a
  directory as the link instead of following it into its target, so a restored
  `node_modules` runs and a Yarn workspace keeps its links. Fingerprint
  checksums still see what the shell lists, so `.task` state is unaffected.
- **Failed cache imports invalidate saved task status.** After an import
  fails, tasks rebuild instead of trusting partially restored outputs. If
  saved status cannot be removed, Task reports that cleanup failure too.

## v4.6.0 - 2026-09-18

- **`run: once` tasks run once across direct and nested includes.**
  When one Taskfile is included both directly and through another include —
  the root includes `lib`, and also `app`, which itself includes `lib` — a
  `run: once` task in it now runs once for both paths instead of once per
  path. Go Task v3 treats the nested copy as a separate task.
- **A task included under several namespaces shares one cache entry.**
  vk-registry entries, the build-once lock and the archive's task annotation
  are keyed by the task's name in the Taskfile that defines it, so the copy
  reached through a nested include restores what the directly included copy
  built instead of rebuilding it. Tasks that differ only in their `vars`, or
  in the `vars` their include passes, keep separate entries. Every vk-registry
  tag changes — the include namespace and `label` no longer appear in it — and
  an archive saved by a namespaced or labelled task is rebuilt once, so the
  first build on this version repopulates the cache. `.task` fingerprints are
  unaffected.

## v4.5.0 - 2026-09-05

- **A Linux release stays rebuildable for good.** The pinned build environment
  is now one locked package set whose sources stay fetchable indefinitely,
  replacing hand-pinned Alpine packages that eventually disappear. Rebuilding a
  release still needs only Docker and `./build.sh`; the `build-info.txt` shipped
  with each Linux release now records a `flake lock:` digest in place of
  `apk pins:`.

## v4.4.0 - 2026-09-03

- **vk-registry caches need only an address and an API key.** A cache model
  can name a vk-registry repository — `vk: registry.example/task-cache` — and
  Task derives the cache entries and the build-once lock from it; an optional
  `namespace` keeps entries built by different toolchains apart, and a `vk`
  that renders empty turns the cache off. The key comes from `api_key`
  or `TASK_VK_API_KEY`; `api_key` also works on an explicit `oci://` cache or
  `vk://` lock, where it outranks any other credential.
- **One vk-registry API key authenticates an explicit `oci://` cache and
  `vk://` lock.** `TASK_VK_API_KEY` is sent as a bearer token to `oci://`
  caches. `vk://` locks without URL credentials use `TASK_VK_LOCK_TOKEN`, then
  fall back to that key or the `TASK_CACHE_OCI_USER` / `TASK_CACHE_OCI_PASSWORD`
  pair.
- **Authenticate transparent-zstd capability probes.** The `oci://` cache now
  sends registry credentials when negotiating transparent-zstd uploads, so
  credential-gated vk-registry servers use that mode instead of
  compressed-digest uploads.
- **A `vk://` lock is released when its task finishes.** It used to stay held
  until its lease expired, making a peer waiting on the same build wait up to
  30 s longer than needed.
- **An `oci://` cache miss is quiet.** A tag the registry does not have was
  reported as a registry error in verbose output on every miss.
- **A `vk://` lock names its holder's machine** on the registry even when the
  shell did not export `HOSTNAME`, as under a CI job.

## v4.3.2 - 2026-09-02

- **Match files with trailing `**` globs** — `target/**` now matches the same
  files as `target/**/*`, restoring Go Task and bash `globstar` compatibility
  for `sources:`, `generates:`, and `exclude:` patterns. These patterns
  previously matched nothing.
- **Exclude build directories without scanning them** — an `exclude:` such as
  `target/**/*` skips matching trees, keeping large excludes fast without
  changing the matched file set.

## v4.3.1 - 2026-08-30

- **`task --help` lists the flags** — it now prints the usage line, every
  option with its description, and the hint to run `task --list-all`, instead of
  a short blurb that pointed back at `--help` for the flag list.

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

---
title: Taskfile Schema Reference
description: A reference for the Taskfile schema
outline: deep
---

# Taskfile Schema Reference

This page documents all available properties and types for the Taskfile schema
version 3, based on the
[official JSON schema](https://taskfile.dev/schema.json).

## Root Schema

The root Taskfile schema defines the structure of your main `Taskfile.yml`.

### `version`

- **Type**: `string` or `number`
- **Required**: Yes
- **Valid values**: `"3"`, `3`, or any valid semver string
- **Description**: Version of the Taskfile schema

```yaml
version: '3'
```

### `output`

- **Type**: `string` or `object`
- **Default**: `interleaved`
- **Options**: `interleaved`, `group`, `prefixed`
- **Description**: Controls how task output is displayed

```yaml
# Simple string format
output: group

# Advanced object format
output:
  group:
    begin: "::group::{{.TASK}}"
    end: "::endgroup::"
    error_only: false
```

### [`includes`](#include)

- **Type**: `map[string]Include`
- **Description**: Include other Taskfiles

```yaml
includes:
  # Simple string format
  docs: ./Taskfile.yml

  # Full object format
  backend:
    taskfile: ./backend
    dir: ./backend
    optional: false
    flatten: false
    internal: false
    aliases: [api]
    excludes: [internal-task]
    vars:
      SERVICE_NAME: backend
    checksum: abc123...
```

### [`vars`](#variable)

- **Type**: `map[string]Variable`
- **Description**: Global variables available to all tasks

```yaml
vars:
  # Simple values
  APP_NAME: myapp
  VERSION: 1.0.0
  DEBUG: true
  PORT: 8080
  FEATURES: [auth, logging]

  # Dynamic variables
  COMMIT_HASH:
    sh: git rev-parse HEAD

  # Variable references
  BUILD_VERSION:
    ref: .VERSION

  # Map variables
  CONFIG:
    map:
      database: postgres
      cache: redis
```

### `env`

- **Type**: `map[string]Variable`
- **Description**: Global environment variables

```yaml
env:
  NODE_ENV: production
  DATABASE_URL:
    sh: echo $DATABASE_URL
```

### [`tasks`](#task)

- **Type**: `map[string]Task`
- **Description**: Task definitions

```yaml
tasks:
  # Simple string format
  hello: echo "Hello World"

  # Array format
  build:
    - go mod tidy
    - go build ./...

  # Full object format
  deploy:
    desc: Deploy the application
    cmds:
      - ./scripts/deploy.sh
```

### `silent`

- **Type**: `bool`
- **Default**: `false`
- **Description**: Suppress task name and command output by default

```yaml
silent: true
```

### `dotenv`

- **Type**: `[]string`
- **Description**: Load environment variables from .env files. When the same
  variable is defined in multiple files, the first file in the list takes
  precedence.

```yaml
dotenv:
  - .env.local # Highest priority
  - .env # Lowest priority
```

### `run`

- **Type**: `string`
- **Default**: `always`
- **Options**: `always`, `once`, `when_changed`
- **Description**: Default execution behavior for tasks

```yaml
run: once
```

### `interval`

- **Type**: `string`
- **Default**: `100ms`
- **Pattern**: `^[0-9]+(?:m|s|ms)$`
- **Description**: Watch interval for file changes

```yaml
interval: 1s
```

### `set`

- **Type**: `[]string`
- **Options**: `allexport`, `a`, `errexit`, `e`, `noexec`, `n`, `noglob`, `f`,
  `nounset`, `u`, `xtrace`, `x`, `pipefail`
- **Description**: POSIX shell options for all commands

```yaml
set: [errexit, nounset, pipefail]
```

### `shopt`

- **Type**: `[]string`
- **Options**: `expand_aliases`, `globstar`, `nullglob`
- **Description**: Bash shell options for all commands

```yaml
shopt: [globstar]
```

## Include

Configuration for including external Taskfiles.

### `taskfile`

- **Type**: `string`
- **Required**: Yes
- **Description**: Path to the Taskfile or directory to include

```yaml
includes:
  backend: ./backend/Taskfile.yml
  # Shorthand for above
  frontend: ./frontend
```

### `dir`

- **Type**: `string`
- **Description**: Working directory for included tasks

```yaml
includes:
  api:
    taskfile: ./api
    dir: ./api
```

### `optional`

- **Type**: `bool`
- **Default**: `false`
- **Description**: Don't error if the included file doesn't exist

```yaml
includes:
  optional-tasks:
    taskfile: ./optional.yml
    optional: true
```

### `flatten`

- **Type**: `bool`
- **Default**: `false`
- **Description**: Include tasks without namespace prefix

```yaml
includes:
  common:
    taskfile: ./common.yml
    flatten: true
```

### `internal`

- **Type**: `bool`
- **Default**: `false`
- **Description**: Hide included tasks from command line and `--list`

```yaml
includes:
  internal:
    taskfile: ./internal.yml
    internal: true
```

### `aliases`

- **Type**: `[]string`
- **Description**: Alternative names for the namespace

```yaml
includes:
  database:
    taskfile: ./db.yml
    aliases: [db, data]
```

### `excludes`

- **Type**: `[]string`
- **Description**: Tasks to exclude from inclusion

```yaml
includes:
  shared:
    taskfile: ./shared.yml
    excludes: [internal-setup, debug-only]
```

### `vars`

- **Type**: `map[string]Variable`
- **Description**: Variables to pass to the included Taskfile

```yaml
includes:
  deploy:
    taskfile: ./deploy.yml
    vars:
      ENVIRONMENT: production
```

## Variable

Variables support multiple types and can be static values, dynamic commands,
references, or maps.

### Static Variables

```yaml
vars:
  # String
  APP_NAME: myapp
  # Number
  PORT: 8080
  # Boolean
  DEBUG: true
  # Array
  FEATURES: [auth, logging, metrics]
  # Null
  OPTIONAL_VAR: null
```

### Dynamic Variables (`sh`)

```yaml
vars:
  COMMIT_HASH:
    sh: git rev-parse HEAD
  BUILD_TIME:
    sh: date -u +"%Y-%m-%dT%H:%M:%SZ"
```

### Variable References (`ref`)

```yaml
vars:
  BASE_VERSION: 1.0.0
  FULL_VERSION:
    ref: .BASE_VERSION
```

### Map Variables (`map`)

```yaml
vars:
  CONFIG:
    map:
      database:
        host: localhost
        port: 5432
      cache:
        type: redis
        ttl: 3600
```

### Variable Ordering

Variables can reference previously defined variables:

```yaml
vars:
  GREETING: Hello
  TARGET: World
  MESSAGE: '{{.GREETING}} {{.TARGET}}!'
```

## Task

Individual task configuration with multiple syntax options.

### Simple Task Formats

```yaml
tasks:
  # String command
  hello: echo "Hello World"

  # Array of commands
  build:
    - go mod tidy
    - go build ./...

  # Object with cmd shorthand
  test:
    cmd: go test ./...
```

### Task Properties

#### `cmds`

- **Type**: `[]Command`
- **Description**: Commands to execute

```yaml
tasks:
  build:
    cmds:
      - go build ./...
      - echo "Build complete"
```

#### `cmd`

- **Type**: `string`
- **Description**: Single command (alternative to `cmds`)

```yaml
tasks:
  test:
    cmd: go test ./...
```

#### `deps`

- **Type**: `[]Dependency`
- **Description**: Tasks to run before this task

```yaml
tasks:
  # Simple dependencies
  deploy:
    deps: [build, test]
    cmds:
      - ./deploy.sh

  # Dependencies with variables
  advanced-deploy:
    deps:
      - task: build
        vars:
          ENVIRONMENT: production
      - task: test
        vars:
          COVERAGE: true
    cmds:
      - ./deploy.sh

  # Silent dependencies
  main:
    deps:
      - task: setup
        silent: true
    cmds:
      - echo "Main task"

  # Loop dependencies
  test-all:
    deps:
      - for: [unit, integration, e2e]
        task: test
        vars:
          TEST_TYPE: '{{.ITEM}}'
    cmds:
      - echo "All tests completed"
```

#### `setup`

- **Type**: `[]Dependency`
- **Description**: Tasks to run **before** fingerprinting and locking. Setup
  tasks run unconditionally every time the parent task is invoked — even when
  the parent is already up to date. They execute sequentially in the order
  listed.

Use `setup` for operations that must complete before the parent's source
fingerprint can be computed, such as code generation, version stamping, or
fetching external inputs. Setup task outputs are **not** merged into the
parent's fingerprint, and `from: deps` does not read `setup`; name the setup
task in the parent's `sources` (`- task: generate-version`) if you need that.

```yaml
tasks:
  build:
    setup:
      - generate-version
    sources:
      - version.txt
      - src/**/*.go
    generates:
      - bin/app
    cmds:
      - go build -o bin/app ./cmd

  generate-version:
    cmds:
      - git describe --tags > version.txt
```

#### `desc`

- **Type**: `string`
- **Description**: Short description shown in `--list`

```yaml
tasks:
  test:
    desc: Run unit tests
    cmds:
      - go test ./...
```

#### `summary`

- **Type**: `string`
- **Description**: Detailed description shown in `--summary`

```yaml
tasks:
  deploy:
    desc: Deploy to production
    summary: |
      Deploy the application to production environment.
      This includes building, testing, and uploading artifacts.
```

#### `prompt`

- **Type**: `string` or `[]string`
- **Description**: Prompts shown before task execution

```yaml
tasks:
  # Single prompt
  deploy:
    prompt: "Deploy to production?"
    cmds:
      - ./deploy.sh

  # Multiple prompts
  deploy-multi:
    prompt:
      - "Are you sure?"
      - "This will affect live users!"
    cmds:
      - ./deploy.sh
```

#### `aliases`

- **Type**: `[]string`
- **Description**: Alternative names for the task

```yaml
tasks:
  build:
    aliases: [compile, make]
    cmds:
      - go build ./...
```

#### `sources`

- **Type**: `[]string` or `[]Glob`
- **Description**: Source files to monitor for changes

```yaml
tasks:
  build:
    sources:
      - '**/*.go'
      - go.mod
      # With exclusions
      - exclude: '**/*_test.go'
    cmds:
      - go build ./...
```

An entry can also copy another task's sources with `from:` or `task:`; see
[`cache`](#cache).

#### `generates`

- **Type**: `[]string` or `[]Glob`
- **Description**: Files generated by this task

```yaml
tasks:
  build:
    sources: ['**/*.go']
    generates:
      - './app'
      - exclude: '*.debug'
    cmds:
      - go build -o app ./cmd
```

An entry can also copy another task's generates with `from:` or `task:`; see
[`cache`](#cache).

When collecting generated files for a cache archive, wildcards include hidden
entries such as `node_modules/.bin`. Recursive patterns such as
`node_modules/**/*` store directory symlinks as links without traversing their
targets. A literal directory segment can still resolve through a symlink, as
in `node_modules/workspace/*`. Exclusions use the same matching rules.
Fingerprint checksums retain their existing matching rules, so existing
`.task` state stays valid.

#### `status`

- **Type**: `[]string`
- **Description**: Commands to check if task should run

```yaml
tasks:
  install-deps:
    status:
      - test -f node_modules/.installed
    cmds:
      - npm install
      - touch node_modules/.installed
```

#### `preconditions`

- **Type**: `[]Precondition`
- **Description**: Conditions that must be met before running

```yaml
tasks:
  # Simple precondition (shorthand)
  build:
    preconditions:
      - test -d ./src
    cmds:
      - go build ./...

  # Preconditions with custom messages
  deploy:
    preconditions:
      - sh: test -n "$API_KEY"
        msg: 'API_KEY environment variable is required'
      - sh: test -f ./app
        msg: "Application binary not found. Run 'task build' first."
    cmds:
      - ./deploy.sh
```

#### `if`

- **Type**: `string`
- **Description**: Shell command to conditionally execute the task. If the
  command exits with a non-zero code, the task is skipped (not failed).

```yaml
tasks:
  # Task only runs in CI environment
  deploy:
    if: '[ "$CI" = "true" ]'
    cmds:
      - ./deploy.sh

  # Using Go template expressions
  build-prod:
    if: '{{eq .ENV "production"}}'
    cmds:
      - go build -ldflags="-s -w" ./...
```

#### `dir`

- **Type**: `string`
- **Description**: The directory in which this task should run
- **Default**: If the task is in the root Taskfile, the default `dir` is
  `ROOT_DIR`. For included Taskfiles, the default `dir` is the value specified in
  their respective `includes.*.dir` field (if any).

```yaml
tasks:
  current-dir:
    dir: '{{.USER_WORKING_DIR}}'
    cmd: pwd
```

#### `requires`

- **Type**: `Requires`
- **Description**: Required variables with optional enum validation

```yaml
tasks:
  # Simple requirements
  deploy:
    requires:
      vars: [API_KEY, ENVIRONMENT]
    cmds:
      - ./deploy.sh

  # Requirements with enum validation
  advanced-deploy:
    requires:
      vars:
        - API_KEY
        - name: ENVIRONMENT
          enum: [development, staging, production]
        - name: LOG_LEVEL
          enum: [debug, info, warn, error]
    cmds:
      - echo "Deploying to {{.ENVIRONMENT}} with log level {{.LOG_LEVEL}}"
      - ./deploy.sh
```

See [Prompting for missing variables interactively](/docs/guide#prompting-for-missing-variables-interactively)
for information on enabling interactive prompts for missing required variables.

#### `watch`

- **Type**: `bool`
- **Default**: `false`
- **Description**: Automatically run task in watch mode

```yaml
tasks:
  dev:
    watch: true
    cmds:
      - npm run dev
```

#### `cache`

- **Type**: `string | object`
- **Description**: Configures remote caching and distributed locking for a task.
  When specified as a string, it references a named cache model defined in the
  top-level `caches:` map. When specified as an object, it can inherit from a
  model and override individual fields.

Cache fields:

| Field       | Type     | Description                                                          |
| ----------- | -------- | -------------------------------------------------------------------- |
| `inherit`   | `string` | Name of a cache model to inherit from                                |
| `enabled`   | `bool`   | Explicitly enable or disable the cache block                         |
| `vk`        | `string` | Template string resolving to a vk-registry repository; see below     |
| `api_key`   | `string` | Template string resolving to a bearer token (a vk-registry API key)  |
| `namespace` | `string` | Template string prefixing the entries and locks under `vk`           |
| `url`       | `string` | Template string resolving to the cache URL (`file://`, `oci://`)     |
| `lock`      | `string` | Lock URL (`file://`, `redis://`, `vk://`, `vks://`); see below       |
| `ttl`       | `string` | TTL for cached assets (e.g. `48h`, `7d`); default `48h`             |

**vk-registry**: `vk: host[:port]/repo` names one vk-registry repository that
serves both the cache and the build-once lock, and derives the two URLs below
from it — the entry `oci://host/repo:<namespace>-<task>-<checksum>` (made
tag-safe and length-capped) and the lock `vks://host/repo/<namespace>`. `<task>`
is the task's name in the Taskfile that defines it — no include namespace, no
`label` — so a task included under several namespaces shares one entry; the
checksum carries the task's and the include's `vars`, so tasks that render
different commands from the same text stay apart. The credential is `api_key` (a
bearer token, minted by the registry), else `$TASK_VK_API_KEY` (the lock also
honours `$TASK_VK_LOCK_TOKEN` in between); the trust anchor for a private
certificate is `$TASK_CACHE_OCI_CA`. `namespace` keeps entries built by
different toolchains apart (it has no effect without `vk`). A `vk` that renders
empty — its CI variable unset on a developer machine — disables the block; one
that renders to anything but `host[:port]/repo` is an error. `vk` cannot be
combined with `url` or `lock`. `api_key` also works with an explicit `oci://`
URL and `vk://` lock, where it outranks every other credential.

```yaml
caches:
  default:
    vk: '{{.CI_VK_REGISTRY}}'      # e.g. registry.example/task-cache
    namespace: '{{.BUILDER_TAG}}'
```

Supported `url` (storage) schemes:

- `file://<dir>` — local archives.
- `oci://[user:password@]host/repo:tag[?ca=<file>][&cas=<dir>][&plainhttp=1]` —
  a chunk-deduplicated artifact on any OCI registry. Environment credentials
  are `$TASK_VK_API_KEY` (a bearer token, such as a vk-registry API key) or
  `$TASK_CACHE_OCI_USER` / `$TASK_CACHE_OCI_PASSWORD` (Basic), with the token
  taking precedence. `$TASK_CACHE_OCI_CA` and `$TASK_CACHE_OCI_CAS_DIR` provide
  the other environment settings. Against a vk-registry server, Task also
  negotiates its transparent-zstd upload mode automatically.

OCI cache requests retry connection failures, timeouts and interrupted transfers
up to three times, waiting 2, 4 and 8 seconds between attempts. Each retry starts
the transfer again. HTTP errors, malformed responses and digest mismatches are
not retried. If the registry remains unreachable, Task continues without its
cache.

Supported `lock` schemes:

- `file://<dir>` — local lockfiles (single host only).
- `redis://[user:pass@]host[:port]/<prefix>` — Redis `SET NX EX` with a heartbeat.
- `vk://host[:port]/<prefix>` (`vks://host[:port]/<prefix>[?ca=<file>]` for
  HTTPS) — the vk-registry HTTP lock API, so one vk-registry serves both the
  `oci://` cache and the lock without Redis. Credential precedence is the
  block's `api_key`, URL Basic, `$TASK_VK_LOCK_TOKEN`, `$TASK_VK_API_KEY`, then
  `$TASK_CACHE_OCI_USER` / `$TASK_CACHE_OCI_PASSWORD`, allowing one token or
  account to cover both APIs. Credentials travel in the clear over `vk://` on
  every acquire, renew and release; use `vks://` outside trusted networks. A
  `vks://` lock adds `?ca=<file>`, or `$TASK_CACHE_OCI_CA` when unset, to the
  system trust store.

All template fields (`url`, `lock`, `vk`, `api_key`, `namespace`, `enabled`,
`lock_timeout`) support standard Task variables plus `{{.TASK}}`,
`{{.CHECKSUM}}`, and the `urlsafe` template function.

Each renders in the dialect of the file it was written in: a `caches:` model in
the file defining it, a task-level override in the task's own file.

If the remote lock (e.g. Redis) is unavailable, Task logs a warning and falls
back to a local file lock so the task still runs.

Both `sources` and `generates` entries support a `from:` directive that
copies entries from related tasks. Supported values are `deps` (direct
dependencies) and `cmds` (cmd task-calls). This allows wrapper tasks to
participate in fingerprinting and caching without duplicating glob patterns:

```yaml
# Define reusable cache models at the top level
caches:
  default:
    enabled: '{{ne .CACHE_URL ""}}'
    url: '{{.CACHE_URL}}/cache:{{urlsafe .TASK}}-{{.CHECKSUM}}.zip'
    lock: '{{.CACHE_URL}}/lock:{{urlsafe .TASK}}-{{.CHECKSUM}}'

tasks:
  build:
    cache: default
    sources:
      - src/**/*.go
    generates:
      - bin/app
    cmds:
      - go build -o bin/app ./cmd

  # Wrapper task: inherits both sources and generates from its deps.
  build-all:
    cache: default
    sources:
      - from: deps
    generates:
      - from: deps
    deps:
      - build

  # Task that delegates via cmds: inherits from cmd task-calls.
  build-lang:
    sources:
      - from: cmds
    generates:
      - from: cmds
    cmds:
      - task: build
```

`from: cmds` reads a deferred `task:` command as it runs after a success; one
whose name or `vars` mention `{{.EXIT_CODE}}`, or `{{.CHECKSUM}}` in a task
with `from:` or `task:` entries itself, is an error, as is one whose template
fails to render (a run would ignore it): list its targets explicitly.

A `task:` entry names one task, which need not be a dependency or be called.
It takes optional `vars`, as a dependency call does, and nothing else. The
referenced task does not run. In `sources`, it stands for what running that
task would read: its `sources`, and those of every task it runs through
`setup`, `deps` and `task:` commands (`for:` loops and deferred calls
included), transitively, each with the vars that call passes. In `generates`
it copies the named task's `generates` only, its products, so a cache entry
holds those and not the intermediates the tasks it runs leave. `from: deps`
and `from: cmds` copy the same field of the direct dependencies or called
tasks, one level. Use `task:` to keep a task that reads another task's inputs
(tests, linters, packaging) in step with that task's `sources`, which
`sandbox` otherwise reports as soon as they drift:

```yaml
tasks:
  build:
    sources: ["src/**/*.go", go.mod, go.sum]
    cmds:
      - go build ./...

  test:
    sources:
      - task: build
      - "tests/**/*.go"
    sandbox: true
    cmds:
      - go test ./...
```

In an included Taskfile, the name resolves like a `deps` entry: within the
include, or from the root with a leading `:`. A name that does not exist, or
that renders empty, is an error. References are followed through the
referenced tasks' own `from:` and `task:` entries. Each task counts once with
the same vars, so a cycle between references is no error; one called with
other vars counts again (a task calling itself for another language covers
both).

What a task runs is found without running anything, conservatively: an `if:`
on a task or command, `requires`, prompts and a command's `platforms:` are
ignored, so a call they would skip still counts, and a task it names must
exist (exit code 200, naming the referring task and the calls leading to the
missing one). Only a task-level `platforms:` excluding the host leaves out a
task that would be run, with what it runs, and without compiling it; a task a
`task:` entry names is included whatever its `platforms:`. A `task:` command
whose name renders empty runs nothing, and counts for nothing. A deferred call
counts as it runs after a success; one whose name or `vars` mention
`{{.EXIT_CODE}}`, or `{{.CHECKSUM}}` in a task that has `task:` or `from:`
entries in its own `sources` or `generates`, is an error, as is one whose
template fails to render, which a run would ignore: list its targets with
their own `task:` entries. A recursion whose end depends on what the walk does
not evaluate (an `if:`) stops at 256 calls of one task, or 1000 calls in all,
with exit code 204. The `sh:` variables of the tasks reached run as for a run,
unsandboxed.

A folded task's `exclude:` entries remove only that task's own matches, never
those of the tasks it runs or folds in, another folded task's or the referring
task's. The referring task's own `exclude:` entries apply to everything it
folds in.

Entries folded in by `from:` or `task:` count for the up-to-date check and the
cache, but two things in `cmds` see only the task's own entries:

- `for: sources` and `for: generates` iterate the task's own entries; the
  folded-in ones are left out.
- `{{.CHECKSUM}}` in `cmds`, `deps` and `setup` hashes the task's own
  entries, so it does not change when a folded-in file does. Deferred
  commands, `preconditions` and the cache block see the `{{.CHECKSUM}}` the
  fingerprint records, which covers them (empty when they resolve to no
  sources).

#### `sandbox`

- **Type**: `bool | object`
- **Description**: Restricts the task's commands, and the tasks they call, to
  the files the task declares. Inside the root directory a command can read
  only the task's `sources`, the Taskfiles and `reads`; reading any other file
  fails with "Permission denied". This checks that `sources` lists every input
  the build reads. A missed input otherwise leaves a stale up-to-date result or
  cache entry. Outside the root directory nothing is restricted, except in the
  directories above it (see below). Linux only.

| Field    | Type                       | Description                                                                                   |
| -------- | -------------------------- | --------------------------------------------------------------------------------------------- |
| `reads`  | `[]glob`                   | Files the commands may also read, not fingerprinted (e.g. `.git/**`); no `from:` or `task:`   |
| `writes` | `[]string \| {task, vars}` | Directories the commands may change freely, relative to the task directory, or `{task, vars}` |

What a command can change inside the root directory:

- a directory whose files are all readable, so commands can create files next
  to the sources. Symlinks do not count: reading through one is checked on the
  file it leads to;
- the directories `generates` writes in, the `.task` directory and `writes`
  (created when missing). Reads inside them are not restricted, so task warns
  when a `generates` directory also holds sources (`generates: [src/main.o]`
  opens all of `src/`);
- nothing else: a command cannot create a file in a directory that holds files
  it may not read.

The root directory itself is never writable, even when every file in it is
readable: nothing can be created, removed or renamed directly in it, and its
own files can only be read. A file generated directly in it
(`generates: [app]`) cannot be created, and task warns to put it in a
subdirectory (`generates: [bin/app]`). A `generates` directory or `writes`
entry that is the root directory or above it (`generates: [../app.deb]`) is
ignored, with a warning.

The directories above the root directory can be listed, but no entry can be
created, removed or renamed directly in them: Landlock cannot grant that without
granting it throughout the root directory. A root directory placed directly in
`$HOME` or `/tmp` therefore cannot create files there, nor use an entry made
there after the task started; deeper directories are not affected. A `generates`
directory outside the root directory is created beforehand only when it is
missing directly in one of them; elsewhere (`/opt/app/*`) the commands create it
themselves, if they can. When the system temporary directory contains the root
directory, or is inside it, commands get `TMPDIR` set to a directory of their
own under `.task/tmp`, replacing any `TMPDIR` the task sets. The directory is
removed after the commands finish, or by a later sandboxed run if task was
killed.

The sandboxed task's own `deps` run outside its sandbox, because they run
whether the task is up to date or not; a task it calls still runs its `deps`
inside it. A task deduplicated by `run: once` or `run: when_changed` and
reached both from a sandboxed task and from outside it runs only once, under
the sandbox of whichever caller starts it first. `TASK_SANDBOX` selects how the
sandbox is enforced; see the
[environment reference](environment.md#task_sandbox).

Under the seccomp supervisor, any access it denies fails the task and records
no fingerprint or cache entry, even when the command recovered from it
(`cat secret || true`) or ignores its errors (`ignore_error`, `defer`). A
command's `if` runs under the supervisor when the sandbox is enforced, so a
denied condition fails the task too; under Landlock, when the supervisor is
unavailable, it runs unsandboxed with a warning. Under Landlock, a denial is
only the command's own "Permission denied": a command that fails on it fails
the task as usual, but one that recovers from it, or whose errors are ignored,
passes unnoticed.
`TASK_SANDBOX=audit` reports instead of failing.

The sandbox checks a build's declared inputs; it is not a security boundary.
Both mechanisms set `PR_SET_NO_NEW_PRIVS` on the commands, so setuid and
file-capability programs (`sudo`, `ping` on some systems) do not gain their
privileges and typically fail.

```yaml
tasks:
  build:
    sources: ["src/**/*.c", Makefile]
    generates: ["build/**/*"]
    sandbox:
      reads: [".git/**"]
      writes: [.ccache]
    cmds:
      - make
```

The sandbox also covers the tasks the commands call, through `task:` in `cmds`
or a nested `task` process (a script running `task rust:build`). Those write
their own `generates`, so a `writes` entry can name a task: `{task: X}`, with
optional `vars` as a dependency call, grants what X and every task X runs (as
a `task:` entry of `sources` finds them) declare:

- the directories their `generates` write in (with their own `from:` and
  `task:` generates entries), as they are for X itself. A pattern writes in
  its part before the first wildcard (`target/**/*` in `target/`), a plain
  file path in its directory (`bin/app` in `bin/`);
- their own `sandbox.writes` directories, those directories themselves (not
  their parents), created beforehand. One at or above the root directory is
  ignored with a warning naming the task that declares it, as for the task's
  own;
- their `dotenv` files, readable (not fingerprinted), which a nested `task`
  process reads. A directory whose files are all readable stays writable as
  usual.

Their `sandbox.reads` are not granted. Reads inside the granted directories are
not restricted either, but one that also holds sources draws no warning (a
translation catalog compiled next to its `.po` file): where X writes is X's to
change, and a sandboxed X warns about its own `generates`. The grants apply to
every command of the task, not only to the call of X, and calls behind an
`if:` count too. An unknown name, or one that renders empty, is an error. The
entry grants access only: it changes neither the task's fingerprint nor its
cache key. One whose tasks declare no `generates` or `writes` grants nothing,
with a warning. `--verbose` lists each grant with the calls it comes from
(`sandbox.writes {task: X} → deps "Y": writes "cache"`); a denial names only
the command that was denied.

The entry's `vars` must render the same `generates` as the call the commands
make: the `vars` of a `task:` command, or the `VAR=value` arguments of a nested
`task X VAR=value`. The caller's own command-line variables apply to the
reference, the nested process's arguments do not. A mismatch makes another
directory writable, and the sandbox usually denies the callee's writes.

A task that runs X also depends on X's inputs, so it lists them in its own
`sources` with `- task: X`, which makes them, and those of the tasks X runs,
readable and fingerprints them. `sandbox.reads` takes no `task:` entry for
that: inputs read without being fingerprinted would leave a stale up-to-date
result or cache entry.

A nested `task` process is found as if it were called with `task:` from the
same Taskfile. What it does differently is not followed: it inherits the
command's environment, dotenv values and working directory, compiles its
variables anew, gets no prompted values, and sees the files earlier commands
left. The declarations must match what it does. A divergence can show as a
denial or as a fingerprint that misses an input, or go unnoticed, for instance
when it reads another file inside a granted `writes` directory, which is
neither restricted nor fingerprinted. A nested run of another Taskfile (`-d`,
`-t`) is not followed at all; list what it reads and writes explicitly. The
sources of X's deps that X itself reads once they are generated (X's
`sources` matching a dep's `generates`) change while the task runs, which is
reported, and no fingerprint or cache entry is recorded.

```yaml
tasks:
  debstage:
    sources:
      - debian/**
      - task: package
        vars: { LANG: fr }
    generates: [debian/tmp/**/*]
    sandbox:
      writes:
        - build/tmp
        - task: package
          vars: { LANG: fr }
    cmds:
      # runs `task package LANG=fr`
      - debian/rules binary

  # Its deps' sources, generates and writes are covered too.
  package:
    deps:
      - rust:build
      - task: i18n:build
        vars: { LANG: '{{.LANG}}' }
    generates: [dist/*.tar]
    cmds:
      - tar cf dist/app-{{.LANG}}.tar target/release/app po/{{.LANG}}.mo
```

#### `platforms`

- **Type**: `[]string`
- **Description**: Platforms where this task should run

```yaml
tasks:
  windows-build:
    platforms: [windows]
    cmds:
      - go build -o app.exe ./cmd

  unix-build:
    platforms: [linux, darwin]
    cmds:
      - go build -o app ./cmd
```

## Command

Individual command configuration within a task.

### Basic Commands

```yaml
tasks:
  example:
    cmds:
      - echo "Simple command"
      - ls -la
```

### Command Object

```yaml
tasks:
  example:
    cmds:
      - cmd: echo "Hello World"
        silent: true
        ignore_error: false
        platforms: [linux, darwin]
        set: [errexit]
        shopt: [globstar]
```

### Task References

```yaml
tasks:
  example:
    cmds:
      - task: other-task
        vars:
          PARAM: value
        silent: false
```

### Deferred Commands

```yaml
tasks:
  with-cleanup:
    cmds:
      - echo "Starting work"
      # Deferred command string
      - defer: echo "Cleaning up"
      # Deferred task reference
      - defer:
          task: cleanup-task
          vars:
            CLEANUP_MODE: full
```

### For Loops

#### Loop Over List

```yaml
tasks:
  greet-all:
    cmds:
      - for: [alice, bob, charlie]
        cmd: echo "Hello {{.ITEM}}"
```

#### Loop Over Sources/Generates

```yaml
tasks:
  process-files:
    sources: ['*.txt']
    cmds:
      - for: sources
        cmd: wc -l {{.ITEM}}
      - for: generates
        cmd: gzip {{.ITEM}}
```

#### Loop Over Variable

```yaml
tasks:
  process-items:
    vars:
      ITEMS: 'item1,item2,item3'
    cmds:
      - for:
          var: ITEMS
          split: ','
          as: CURRENT
        cmd: echo "Processing {{.CURRENT}}"
```

#### Loop Over Matrix

```yaml
tasks:
  test-matrix:
    cmds:
      - for:
          matrix:
            OS: [linux, windows, darwin]
            ARCH: [amd64, arm64]
        cmd: echo "Testing {{.ITEM.OS}}/{{.ITEM.ARCH}}"
```

#### Loop in Dependencies

```yaml
tasks:
  build-all:
    deps:
      - for: [frontend, backend, worker]
        task: build
        vars:
          SERVICE: '{{.ITEM}}'
```

### Conditional Commands

Use `if` to conditionally execute a command. If the shell command exits with a
non-zero code, the command is skipped.

```yaml
tasks:
  build:
    cmds:
      # Only run in production
      - cmd: echo "Optimizing for production"
        if: '[ "$ENV" = "production" ]'
      # Using Go templates
      - cmd: echo "Feature enabled"
        if: '{{eq .ENABLE_FEATURE "true"}}'
      # Inside for loops (evaluated per iteration)
      - for: [a, b, c]
        cmd: echo "processing {{.ITEM}}"
        if: '[ "{{.ITEM}}" != "b" ]'
```

## Shell Options

### Set Options

Available `set` options for POSIX shell features:

- `allexport` / `a` - Export all variables
- `errexit` / `e` - Exit on error
- `noexec` / `n` - Read commands but don't execute
- `noglob` / `f` - Disable pathname expansion
- `nounset` / `u` - Error on undefined variables
- `xtrace` / `x` - Print commands before execution
- `pipefail` - Pipe failures propagate

```yaml
# Global level
set: [errexit, nounset, pipefail]

tasks:
  debug:
    # Task level
    set: [xtrace]
    cmds:
      - cmd: echo "This will be traced"
        # Command level
        set: [noexec]
```

### Shopt Options

Available `shopt` options for Bash features:

- `expand_aliases` - Enable alias expansion
- `globstar` - Enable `**` recursive globbing
- `nullglob` - Null glob expansion

```yaml
# Global level
shopt: [globstar]

tasks:
  find-files:
    # Task level
    shopt: [nullglob]
    cmds:
      - cmd: ls **/*.go
        # Command level
        shopt: [globstar]
```

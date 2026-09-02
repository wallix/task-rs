# task-rs

**A Taskfile v3 runner for builds that need deterministic fingerprints, shared
caches, and cross-machine locking.**

This repository is the WALLIX fork of
[Task](https://taskfile.dev), reimplemented in Rust and distributed as a single
self-contained binary. It is intended for teams that already use Taskfiles but
need stronger guarantees around incremental builds and CI reuse.

The Rust rewrite preserves the Taskfile v3 schema, CLI flags, exit codes, and
observable behaviour of the previous Go implementation. Its black-box test
suite ports the Go test corpus, and its fingerprint checksums are
byte-compatible with the pre-rewrite WALLIX fork, so existing `.task` state
remains valid.

This is not upstream Task. The fork deliberately removes a few features and
changes some semantics; review
[Compatibility and differences](#compatibility-and-differences) before
replacing an upstream installation.

## Quick start

Create `Taskfile.yml`:

```yaml
version: '3'
templater: jinja

tasks:
  test:
    desc: Run the test suite
    cmds:
      - go test ./...

  build:
    desc: Build the application
    sources:
      - '**/*.go'
      - go.mod
      - go.sum
    generates:
      - bin/app
    cmds:
      - go build -o bin/app ./cmd/app
```

Then run:

```console
$ task --list
$ task build
```

The second `task build` is skipped when the inputs, commands, variables, and
generated output still match the saved fingerprint.

Start with the [getting-started guide](docs/getting-started.md), then use the
[user guide](docs/guide.md) for Taskfile discovery, variables, includes, and
command-line usage. The full accepted shape is covered by the
[schema reference](docs/reference/schema.md).

## Installation

Download the archive for your platform from
[GitHub Releases](https://github.com/wallix/task-rs/releases), extract `task`,
or `task.exe` on Windows, and place it on your `PATH`. Archives are published
for Linux, macOS, and Windows on x86-64 and ARM64, with a SHA-256 sidecar for
each archive.

An installed binary can update itself:

```bash
task --update              # latest release
task --update=4.3.0        # a specific release
task --update --check      # check without installing
```

The updater verifies the published checksum and runs the downloaded binary to
confirm its version before replacing the current executable. It asks for
confirmation unless `--yes` is supplied.

To build from source, use the toolchain pinned in `rust-toolchain.toml`:

```bash
cargo install --path crates/task
```

The [installation guide](docs/installation.md) also covers shell completions,
download verification, and reproducible builds.

## Why this fork exists

Taskfiles are useful as a readable, repository-local interface to development
and CI commands. The difficult part is deciding when work can safely be skipped
and making that decision hold when several machines build the same revision.
This fork concentrates on that problem.

### Deterministic incremental builds

A task fingerprint covers its sources, generated files, compiled commands, and
variable data. Source and output staleness are reported separately, and
`task --status` exposes the decision without executing the task:

```bash
task --status build
task --status --json build
```

Only checksum-based fingerprinting is supported. Timestamp and `none` methods
were removed because they weaken the relationship between declared inputs and
the result.

### Build cache

A task can restore its generated files instead of running. Cache URLs are
templates, so the source checksum can be part of the key:

```yaml
version: '3'
templater: jinja

tasks:
  build:
    sources:
      - src/**/*.ts
      - package.json
      - yarn.lock
    generates:
      - dist/**/*
    cache:
      url: 'file:///var/cache/task/build-{{ CHECKSUM }}.zip'
    cmds:
      - yarn build
```

Two storage backends are available:

- `file://` stores a ZIP archive on a local or mounted filesystem.
- `oci://` stores content-defined, zstd-compressed chunks in an OCI registry.
  Unchanged chunks are reused across cache entries, and a local content store
  makes repeated restores incremental.

A [vk-registry](https://github.com/wallix/virtkit) needs only its address: a
`vk:` cache model derives the entry and the build-once lock from the one
repository, authenticated by an API key in `TASK_VK_API_KEY`:

```yaml
caches:
  default:
    vk: '{{.CI_VK_REGISTRY}}'   # registry.example/task-cache; empty = cache off

tasks:
  build:
    cache: default
    sources: [src/**]
    generates: [dist/**]
    cmds: [yarn build]
```

For CI systems that move state as an artifact rather than expose a shared cache
service, fingerprint state and generated files can be exported together:

```bash
task --export-cache state.zip build test
task --import-cache state.zip
```

See [Setting up a cache server](docs/cache-server.md) for an OCI registry
deployment and credential configuration.

### Build-once locking

Tasks with both `sources` and `generates` are protected by a local advisory file
lock. The lock includes the task name and source hash, so equivalent builds are
serialized while different inputs remain independent.

Set `cache.lock` to coordinate across machines:

- `redis://` uses a renewable Redis lease.
- `vk://` and `vks://` use the vk-registry lock API and can share an API key
  with the `oci://` cache; use `vks://` when credentials cross an untrusted
  network.
- `file://` places the lock in an explicitly selected shared directory.

If a distributed lock cannot be acquired because its service is unavailable,
Task warns and falls back to a local lock. If a held lease is lost, it does not
publish the resulting fingerprint or cache entry.

### Setup tasks

`setup` is for preparation that must happen before the parent task is checked.
Setup tasks run sequentially and unconditionally, even when the parent is
already up to date. They do not become part of the parent's fingerprint; use
`run: once` for setup shared by several tasks.

```yaml
version: '3'
templater: jinja

tasks:
  version-file:
    run: once
    cmds:
      - git describe --always > version.txt

  build:
    setup: [version-file]
    sources:
      - version.txt
      - src/**/*.go
    generates:
      - bin/app
    cmds:
      - go build -ldflags "-X main.version=$(cat version.txt)" -o bin/app .
```

### Large generated trees

Hashing every file in a large output directory can cost more than the staleness
check is worth. A `generates` entry may name one representative fingerprint
file while retaining the full glob for cache save and restore:

```yaml
generates:
  - glob: node_modules/**/*
    fingerprint: node_modules/.yarn-state.yml
```

`sources` and `generates` may also inherit entries from direct dependencies or
task calls. This keeps wrapper tasks aligned with the work they aggregate:

```yaml
tasks:
  all:
    deps: [frontend, backend]
    sources:
      - from: deps
    generates:
      - from: deps
```

Use `from: cmds` instead when the related tasks are invoked through `cmds`.
Literal globs and inherited entries can be combined, and duplicates are removed
automatically.

## Templating

New Taskfiles should use the native Jinja dialect explicitly:

```yaml
version: '3'
templater: jinja

vars:
  OUTPUT: dist/{{ OS() }}/app

tasks:
  build:
    cmds:
      - '{% if CI %}echo building in CI{% endif %}'
      - go build -o {{ OUTPUT }} .
```

Jinja mode supports expressions, conditionals, loops, filters, and normal
function-call syntax through
[minijinja](https://github.com/mitsuhiko/minijinja).

The legacy Go `text/template` dialect remains available for compatibility but
is deprecated. Files without an explicit `templater` are auto-detected per
file. Convert them before Go rendering is removed:

```bash
task --migrate          # preview on stdout
task --migrate --write  # rewrite the Taskfile in place
```

See the [templating reference](docs/reference/templating.md) for dialect
selection, supported functions, and migration limitations.

## Compatibility and differences

The compatibility target is Taskfile v3, including its command-line surface
and observable runner behaviour. The following differences from upstream Task
v3 are intentional.

### Removed

- Remote `http://` and `git://` Taskfile includes and their CLI flags.
- Timestamp fingerprinting and the task-level `method` field.
- The `none` fingerprint method.

### Added

- Unconditional, sequential `setup` tasks.
- File and OCI build-cache backends with local or distributed locking.
- Representative fingerprint files for large `sources` or `generates` globs.
- `from: deps` and `from: cmds` inheritance for sources and generated files.
- `--status`, `--export-cache`, `--import-cache`, and self-update commands.
- Native Jinja templating and an automated migration path from Go templates.
- Duplicate YAML task keys are errors instead of last-definition-wins.

### Changed

- Task-defined `env` and `vars` override the inherited process environment by
  default. Set `TASK_X_ENV_PRECEDENCE=0` to restore process-environment
  precedence.
- `--force` applies only to tasks named on the command line. Use `--force-all`
  to force their dependencies as well.
- Dependency cycles fail with the cycle path instead of recursing until the
  process exhausts its stack. Self-calls with a different compiled body remain
  valid; recursion driven only by external state is rejected.
- Ctrl-C is escalated through running commands. `SIGTERM` stops the run
  immediately and exits with status `1`.
- Fingerprints include commands and variables as well as file contents, and
  report source and generated-output staleness independently.

For release-specific compatibility notes, including migration fixes and known
gaps, read the [changelog](CHANGELOG.md).

## Reproducible releases

Linux release binaries are built as static musl executables from pinned inputs
and are reproducible from their tagged source. Each Linux release includes a
build manifest containing the binary digest. macOS and Windows binaries are
reproducible only on the same pinned runner image.

The exact guarantees and verification commands are documented in
[Reproducible builds](docs/reference/reproducible-builds.md).

## Contributing

The workspace contains the runner library (`taskcore`), the CLI (`task`), and
the OCI content-addressed store (`ocicas`). A normal edit loop uses the pinned
Rust toolchain:

```bash
cargo check -p taskcore
cargo test -p taskcore --lib
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Run the complete compatibility suite before submitting a change:

```bash
cargo test --workspace
```

Changes to Taskfile behaviour must preserve upstream compatibility or document
an intentional divergence. See [AGENTS.md](AGENTS.md) for the architecture,
development workflow, and review constraints.

## License

[Apache License 2.0](LICENSE)

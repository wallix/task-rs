# AGENTS.md

This file provides guidance to AI coding assistants (Claude Code, Copilot, etc.)
when working with code in this repository.

## Sourced Assertions

Verify load-bearing factual claims — code, tooling, third-party behaviour —
against the code, the repo docs, or a web search before stating them. Flag
anything unverified, or a deduction.

Badge in chat output only (never in commit messages, code comments, or committed
files), inline right after the claim, with a `path:line` cite for code/doc:

- `✅ code` / `✅ doc` `path:line` — verified in the code / repo docs
- `✅ web` URL — verified online (bare URL, outside the code span; pin to a
  commit SHA, not a branch)
- `💭 deduction` — inferred from verified facts
- `⚠️ unverified` — unverifiable, or from training data

Skip badges on restatements, tool output, and descriptions of your own next
step.

## Project Overview

task-rs — the WALLIX fork of [Task](https://taskfile.dev), reimplemented in Rust
and shipped as a single self-contained `task` binary (static-musl on Linux;
`build.yml` also builds macOS and Windows targets). It was a fork of
[go-task/task](https://github.com/go-task/task) with opinionated changes aimed
at build-system reliability (deterministic fingerprinting, distributed caching
and locking, setup tasks); v4.0.0 is a full Rust rewrite of that fork.

It stays **drop-in compatible** with Taskfile v3: same schema, CLI flags, exit
codes and observable behaviour, verified by a black-box suite that ports the
entire Go test corpus. Fingerprint checksum *values* are byte-identical with the
Go implementation, though `.task` entries are keyed by each task's identity
rather than its namespaced name. Included copies with matching compiled build
inputs share state (diverging from Go's per-namespace files); namespaced task
references still distinguish copies. See
[`README.md`](README.md) for the feature tour and the deliberate differences
from upstream v3.

## Architecture

A Cargo workspace (`Cargo.toml`, edition 2024) with three crates:

- **`crates/taskcore/`** — the runner library and where nearly all the logic
  lives: Taskfile parsing (`reader/`, `ast/`), templating (`templater.rs`,
  `migrate.rs`, minijinja in both Go-`text/template`-compatible and native Jinja
  dialects), variable compilation (`compiler.rs`, `variables.rs`, `env.rs`),
  shell execution (`execext.rs`, on the `brush` shell interpreter crates), the
  execution engine (`executor/` — DAG scheduling, `--watch`, caching),
  fingerprinting (`fingerprint/`), the build cache and its distributed locks
  (`cache/`), and output/logging helpers.
- **`crates/task/`** — the `task` binary: a thin CLI over `taskcore` (`cli.rs`
  for the clap surface, `run.rs` for the entrypoint, plus `init`, `fuzzy`
  task-name matching, and the prompter). `src/templates/default.yml` is the
  `--init` template and `completion/` holds the bash/zsh/fish/PowerShell scripts
  — both embedded in the binary (`include_str!`), with `completion/` also
  shipped in every release archive. Its `tests/` are the black-box parity suite.
- **`crates/ocicas/`** — a content-addressed store with content-defined-chunk
  deduplication (fastcdc + zstd + sha2) backed by an OCI registry, plus the
  vk-registry HTTP lock client. Vendored from virtkit
  (`vk-driver/src/registry.rs`) and decoupled from its microVM specifics;
  **intended to be unified back into a shared crate later**, so keep it free of
  task-specific concerns and keep its dependency pins in step with virtkit's.

Shared test fixtures live in the top-level `testdata/` (one directory per case),
driven by the black-box binary tests in `crates/task/tests/`. `taskcore`'s own
integration tests (`crates/taskcore/tests/`) build their Taskfiles in temporary
directories instead. The one test outside cargo is
[`tests/release-e2e.sh`](tests/release-e2e.sh), the release gate: it drives the
packaged binary rather than the workspace.

User-facing documentation lives in `docs/` — the guide, the getting-started /
installation / FAQ / integrations / cache-server / style-guide /
Taskfile-versions pages, and `reference/` for CLI, schema, templating,
environment and reproducible builds; a user-visible behaviour change updates it
in the same commit.

## Drop-in Compatibility Is a Hard Constraint

Behaviour parity with Go Task v3 is load-bearing, in the same way pinning is for
a build system. Before changing anything user-observable, know which side of the
line it is on:

- **Frozen:** the Taskfile v3 schema, CLI flags and their semantics, exit codes,
  stdout / stderr shape that tests assert on, and the **fingerprint checksum
  algorithm** — a change there silently invalidates every existing `.task` cache
  and must be treated as a breaking change, not a refactor.
- **Deliberately divergent:** the differences already documented in `README.md`
  and `CHANGELOG.md` (removed remote taskfiles and timestamp fingerprinting,
  added `setup` tasks and fingerprint-based `generates`, duplicate-key errors,
  env precedence, Jinja templating). Extending this list is a product decision —
  ask, don't assume.

New behaviour gets a `testdata/` case and a black-box test in
`crates/task/tests/` alongside it. `schema.json` describes the accepted Taskfile
shape and is embedded in the binary (`task --schema`): a schema change and the
parser change belong in the same commit. `schema-taskrc.json` describes the
`.taskrc` shape carried over from the Go fork; nothing in the workspace reads
`.taskrc` yet, so it is a published schema only.

## Development Environment

`rust-toolchain.toml` pins the host channel, clippy and rustfmt.
`.devcontainer/nix/flake.nix` pins the same channel inline for the build image,
where Nix installs it instead of rustup. `./update.sh` keeps both pins in sync;
they must not drift. Releases use the container's native musl target. A plain
host `cargo` is enough for the edit loop.

```bash
cargo build -p task                                  # debug binary
cargo test --workspace --locked                      # full suite (CI parity)
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all                                      # check: --all -- --check
```

The repo also runs on the tool it builds — see [`Taskfile.yml`](Taskfile.yml)
(`task build|test|lint|fmt`), which is itself authored in the native Jinja
dialect.

The build image's whole package set is one Nix closure, pinned by
`.devcontainer/nix/flake.lock`. On a host that has Nix, the same toolchain is
available interactively without Docker:

```bash
nix develop ./.devcontainer/nix      # the build image's toolchain, as a shell
nix build ./.devcontainer/nix#buildEnv
```

### Fast edit/check/test loop

While iterating, prefer `cargo check -p <affected-crate>` and run only the
affected test target rather than the whole workspace:

```bash
cargo check -p taskcore
cargo test -p taskcore --lib fingerprint::
cargo test -p task --test cache
```

Run the full `cargo test --workspace --locked` before calling a change done —
the suite is the safety net for the compatibility constraint above.

### Build / container scripts

`build.sh`, `lint.sh`, `fmt.sh` and `audit.sh` run inside the pinned
devcontainer image (one stage, `task-build`). They use `vk` when available and
fall back to Docker; `--docker` forces Docker.
`package.sh`, `release-notes.sh`, `update.sh`, `tests/release-e2e.sh` and
`install-task.sh` run on the host; `update.sh` additionally needs `rustup`,
`curl` and `docker` with the `buildx` plugin — it resolves the base-image
version and digest over the network and refreshes the flake lock inside a
container, so the host needs no Nix.

```bash
./build.sh [--docker]     # reproducible static-musl binary -> dist/task (+ dist/task.sha256)
           [--target=x86_64|aarch64]   # assert the host's arch; the build is native
           [--package]    # + the release archive and sidecar for this platform
           [--verify]     # rebuild from a pristine copy and assert identical bytes
./package.sh --platform <name> --binary <path>   # deterministic release archive
./release-notes.sh v<X.Y.Z>   # that tag's CHANGELOG section, for the release notes
./lint.sh  [--docker]     # cargo clippy --workspace --all-targets --locked -- -D warnings
./fmt.sh   [--docker]     # cargo fmt (--check to verify)
./audit.sh [--docker]     # cargo-audit against the committed Cargo.lock
./update.sh               # bump the pinned toolchain + re-pin the base image and flake lock
./install-task.sh         # the POSIX-sh installer published for end users
RELEASE_TAG=v<X.Y.Z> tests/release-e2e.sh   # the release gate, against dist/
```

`build.sh` output is a stripped static ELF that links no system C libraries
(musl-static, rustls + ring). Rebuilding from the same commit must reproduce the
same bytes — keep the pinning (toolchain, base image digest, flake lock,
`SOURCE_DATE_EPOCH`, path remapping) intact when touching build inputs.

`build.sh` compiles Linux releases natively for the host architecture.
`package.sh` packs every platform on Linux so archive bytes use one toolchain.
macOS and Windows binaries are reproducible only on the same runner image. See
[`docs/reference/reproducible-builds.md`](docs/reference/reproducible-builds.md),
and update it when the guarantee changes.

### Cutting a release

`release` is the integration branch, and the tag is a *result* of a green
pipeline rather than its trigger. Push the candidate there and `release.yml`
gates it, then fast-forwards `main` to it, pushes `v<X.Y.Z>` and publishes the
GitHub release. A failed candidate is corrected and force-pushed to `release`
(`git push --force-with-lease origin HEAD:release`); no tag existed for it, so
no tag is ever moved. `release` is the disposable branch that absorbs those
rewrites, which is what keeps tags and `main` append-only.

Because `publish` fast-forwards `main` from whatever `release` holds, `release`
is a write path into `main` and must be protected at least as strictly as it —
otherwise the review `main` expects is bypassed. `main` in turn has to let the
workflow's `GITHUB_TOKEN` fast-forward it: a "require a pull request" rule there
rejects that push outright.

1. Insert a `## v<X.Y.Z> - <YYYY-MM-DD>` heading in `CHANGELOG.md` below
   `## Unreleased`, moving the unreleased entries under it and leaving
   `## Unreleased` empty for the next cycle.
2. Set `version` under `[workspace.package]` in the root `Cargo.toml` to the
   same version — every crate inherits it.
3. Refresh `Cargo.lock` (`cargo check --workspace`) and commit it with the bump:
   CI builds `--locked`, so a lock still naming the old version fails the gate.
4. Commit as `task-rs: release <X.Y.Z>` on top of an up-to-date `main`, then
   `git push origin HEAD:release`.

The tag is lightweight, so it carries no tagger metadata of its own and
`git describe` names the commit directly. The tag, the workspace version and the
newest CHANGELOG heading must agree: `prepare` derives the tag from
`Cargo.toml`, and `release-notes.sh` takes the body from the matching
`## v<X.Y.Z>` section.
`cargo test --workspace --locked` is where `version_matches_changelog` ties the
workspace version to that heading.

The pipeline is `prepare` (version, unreleased tag, dated CHANGELOG section,
candidate contains `main`) → `quality` (fmt, clippy, `cargo test --workspace
--locked`, audit) and `build` (all six platforms, both Linux binaries rebuilt
from a clean copy and required to reproduce byte-for-byte) → `e2e`
([`tests/release-e2e.sh`](tests/release-e2e.sh), which unpacks the very archives
that will be published and drives the shipped binary) → `publish`. Nothing is
written to `main` or to a tag until all of them are green, so running the checks
locally first is a convenience, not a safeguard.

`publish` can repeat each step: fast-forward `main`, push the tag, create the
release. After a partial failure, re-run the failed job from the Actions UI;
do not tag by hand.

## Code Quality Config

- **Rust:** rustfmt + clippy, pinned via `rust-toolchain.toml`; edition 2024
  comes from `[workspace.package]` in the root `Cargo.toml`. The workspace
  `[workspace.lints.clippy]` block denies `unwrap_used`, `expect_used`, `panic`,
  `indexing_slicing` and `arithmetic_side_effects` for every member; a new crate
  opts in with `[lints] workspace = true`. Tests relax them locally, not
  globally.
- **Dependencies:** versions are centralized in `[workspace.dependencies]`;
  everything beyond the foundational few (`serde`, `serde_json`, `tokio`,
  `anyhow`) carries a comment justifying why the crate earns its place. Keep
  that convention for anything new. The `brush-*` interpreter crates are the
  current exception to the centralization, pinned directly in
  `crates/taskcore/Cargo.toml`.
- **Shell:** Bash, `set -euo pipefail`, `cd "$(dirname "$0")"` — except
  `install-task.sh`, which is POSIX `sh` with `set -e` because it is fetched and
  run on arbitrary machines.
- **Other files:** `.editorconfig` (tabs by default; 2-space for YAML, TOML,
  JSON, Markdown, shell and web files) and `.prettierrc.yml` (single quotes, no
  trailing commas; Markdown wrapped at 80). Neither is enforced in CI — treat
  them as the convention to follow, not a gate.
- **Dependency audit:** `cargo audit --deny warnings` in CI. If an advisory must
  be ignored, add `.cargo/audit.toml` with the rationale and residual risk
  written out.

## CI

`.github/workflows/`: `ci.yml` (push to `main` + PRs) and `release.yml` (push to
`release`) both call the reusable `quality.yml`, which runs fmt, clippy, `cargo
test --workspace --locked`, and `cargo audit --deny warnings` — one matrix entry
each, all four inside the pinned `task-build` image, so CI uses exactly the
toolchain the release build uses. Generated code **must** pass those checks.
Commit dependency changes with their `Cargo.lock` updates. `build.sh` and
`lint.sh` also pass `--locked`, so local runs reject a stale lockfile too.

Both also call `build.yml` — gated on `quality` in CI, in parallel with it on a
release, where the build matrix is the long pole. CI runs its Linux jobs;
releases run the full matrix with reproducibility verification, then gate on
`tests/release-e2e.sh` before publishing with `release-notes.sh`.

## Commit Messages

See
[`.agents/commit-message-guidelines.md`](.agents/commit-message-guidelines.md)
for the format, scope list, body rules, and changelog rules. In short: one
concern per commit, independently buildable; single-line imperative summary (no
trailing period) with an optional `scope:` prefix (e.g. `taskcore/fingerprint:`,
`ci:`, `build.sh:`); a wrapped body only when the diff does not speak for
itself, kept high-level. A user-visible change updates `CHANGELOG.md` in the
same commit, pitched even higher-level than the message.

## Code Review

Code review is expected on the production branch (`main`): one concern per
commit, every commit independently buildable, and every changed line auditable
at a glance. Review against the conventions in
[`.agents/coding-guidelines.md`](.agents/coding-guidelines.md) and the message
rules in
[`.agents/commit-message-guidelines.md`](.agents/commit-message-guidelines.md).

## Coding Conventions

See [`.agents/coding-guidelines.md`](.agents/coding-guidelines.md) for general
conventions, formatting requirements, and per-language guidelines (Rust, Shell,
YAML/JSON).

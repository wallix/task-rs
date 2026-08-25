# YAML & JSON Coding Guidelines

Applies to: the `testdata/` Taskfile fixtures, the repo-root JSON schemas
(`schema.json`, `schema-taskrc.json`), the GitHub workflows under
`.github/workflows/`, and the project's own `Taskfile.yml`.

See [`../coding-guidelines.md`](../coding-guidelines.md) for general conventions
and formatting requirements that apply to all code.

## Conventions

- Two-space indentation, no tabs (`.editorconfig`). No formatter runs in CI for
  these files — keep them tidy by hand.
- **`testdata/` fixtures are the compatibility contract.** One directory per
  case — some grouped one level deeper, e.g. `testdata/dotenv/missing_env`,
  staged as `stage("dotenv/missing_env")` — holding the `Taskfile.yml` (plus any
  nested Taskfiles or input files the case needs); the harness copies the whole
  directory into a temp dir before running, so a fixture must be self-contained
  and must not depend on the repo layout around it
  (`crates/task/tests/common/mod.rs`).
- Fixtures are inputs, not scratch space: do not commit a file the run is
  supposed to generate — a `.gitignore` in the case directory is the usual way
  to keep it out — and never make a case depend on state another case left
  behind. Cases run in parallel. The exception is a ported upstream fixture that
  ships one (`testdata/checksum/generated-wildcard.txt`): keep it as upstream
  has it.
- Ported Go Task fixtures keep their upstream name and content — that is what
  makes the parity claim checkable. Adapt one only with a comment saying why,
  and add a new case rather than mutating an existing one to fit new behaviour.
- Fixtures default to the Go `text/template` dialect, since that is what
  upstream v3 uses and what parity is measured against. Use `templater: jinja`
  only in a case that is specifically exercising the native dialect.
- **`schema.json` is shipped, not just documentation:** it is embedded into the
  binary with `include_str!` and printed by `task --schema`
  (`crates/task/src/run.rs`). A parser change that widens or narrows the
  accepted Taskfile shape updates it in the same commit, and it stays valid JSON
  Schema draft-07 with a `description` on every new property.
- `schema-taskrc.json` describes the `.taskrc` shape carried over from the Go
  fork. Nothing in the workspace reads `.taskrc` yet, so treat the file as a
  published schema only — if `.taskrc` support ever lands, the parser and this
  schema move together.
- **Workflows:** the reusable `quality.yml` is the single definition of
  fmt/clippy/test/audit; `ci.yml` and `release.yml` call it rather than
  restating the steps, so a release is gated on exactly the checks ordinary CI
  runs. Add a check there once, not in both callers. Pin actions by major
  version tag as the existing steps do, and keep `permissions:` least-privilege.
- **`Taskfile.yml` is dogfooding:** it is authored in the native Jinja dialect
  (`templater: jinja`) and must keep working with the binary built from the same
  commit. A change to it is a change to a user-facing example — keep the task
  set aligned with the `cargo` commands in
  [AGENTS.md](../../AGENTS.md#development-environment).

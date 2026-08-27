# Coding Guidelines

## General Coding Conventions

- Small, surgical diffs. Preserve existing style in untouched code.
- Extend patterns already present rather than inventing new ones.
- Validate assumptions by inspecting files before large changes.
- Do not mass-format unrelated files.
- Favor the standard library over external dependencies. Each new dep adds
  supply-chain surface, version churn, and reader burden. Pull one in only when
  stdlib genuinely lacks the capability, the algorithm is correctness-critical
  and risky to reimplement, or the dep is already transitive. "Slightly more
  ergonomic" is not a reason. Prefer writing the glue. Language-specific rules
  in per-area files.
- Drop-in compatibility with Go Task v3 is a hard constraint: the Taskfile
  schema, CLI flags, exit codes, observable output, and the fingerprint checksum
  algorithm are frozen. Divergences are deliberate and documented in `README.md`
  / `CHANGELOG.md` — adding one is a product decision, not a refactor. See
  [AGENTS.md](../AGENTS.md#drop-in-compatibility-is-a-hard-constraint).
- Release builds must stay reproducible and free-standing: one static-musl
  binary, no system C libraries, byte-identical on a rebuild from the same
  commit. Do not introduce build-time non-determinism (timestamps, host paths,
  network-dependent inputs) — see `build.sh`, `.devcontainer/`, and CI for the
  pinning that must be preserved.

## Formatting Requirements

Generated code **must** pass CI's checks (`.github/workflows/quality.yml`: fmt,
clippy, `cargo test --workspace`, `cargo audit --deny warnings`). Only the Rust
row below is CI-enforced; the rest are conventions to follow by hand.

| Language | Formatter / Linter | Check command | Fix command |
|----------|--------------------|---------------|-------------|
| Rust | rustfmt + clippy (CI-enforced) | `cargo fmt --all -- --check` && `cargo clippy --workspace --all-targets -- -D warnings` | `cargo fmt --all` |
| Shell (*.sh) | — (no formatter configured) | `bash -n <file>`, or `sh -n <file>` for the POSIX scripts | — |
| YAML / JSON / Markdown | `.prettierrc.yml` + `.editorconfig` (advisory — no prettier in CI, and no prettier in the repo or the devcontainer) | — | — |

## Area-Specific Conventions

Each language has its own file under [`coding-guidelines/`](coding-guidelines/):

- [Rust (every crate in the workspace)](coding-guidelines/rust.md)
- [Shell (`*.sh`, build & container scripts)](coding-guidelines/shell.md)
- [YAML & JSON (`testdata/` fixtures, the JSON schemas, workflows,
  `Taskfile.yml`)](coding-guidelines/yaml.md)

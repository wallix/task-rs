# Commit Message Guidelines

These rules describe how commits in this repository are structured. They apply
to humans and AI assistants alike.

## Granularity

Decide commit boundaries before writing the message. Each commit should be:

- Small and focused on a single concern.
- Free of mixed change types — split refactor + feature, or rename + logic
  change, into separate commits.
- Independently buildable and deployable; the tree should be green at every
  commit.

Tests for a feature belong in the same commit as the feature itself; do not
split them off — that includes the `testdata/<case>` fixture a black-box test
drives. A schema change (`schema.json`, `schema-taskrc.json`) belongs in the
same commit as the parser change it describes. The same goes for the changelog:
a commit with a user-visible change updates `CHANGELOG.md` in that commit (see
[Changelog](#changelog) below).

## Format

```
<scope>: <imperative summary>

[optional body, wrapped at ~72 columns]
```

A single-line summary (no trailing period) is enough for most commits. Add a
body only when the diff does not speak for itself, and cap it at **5 non-blank
lines** — one short paragraph or a few bullets. Five is a hard cap, not a
target: most bodies that earn one need two or three lines. A change that cannot
be explained in five belongs in a code comment, or is really two commits. If a
change seems to genuinely need a longer body, ask the user before writing one.

The scope prefix is optional when a change is genuinely repo-wide.

Examples:

```
taskcore/fingerprint: hash sources in sorted order
task: accept --migrate --write on a read-only Taskfile
ocicas: retry a chunk upload on a 429 from the registry
ci: run cargo audit as part of the reusable quality workflow
build.sh: pin the devcontainer base image by digest
apply cargo fmt across the workspace
```

## Summary line rules

1. Imperative mood, present tense ("add", "fix", "update", "remove").
2. ≤ 72 characters preferred (hard cap 80). Shorten if longer.
3. No trailing period; lowercase unless it starts with a proper noun.
4. Avoid vague verbs: "remove dead code", not "cleanup"; "refactor X to Y", not
   "refactor".
5. Security fixes: reference the advisory ID in parentheses, e.g. `(due to
   RUSTSEC-2025-0047)`.

## Scope

Pick one lowercase scope matching the component touched. Common scopes here:

- `taskcore`, `task`, `ocicas` — the three crates. Use a module subscope for
  precision, e.g. `taskcore/executor:`, `taskcore/cache:`,
  `taskcore/templater:`.
- `ci` — GitHub Actions workflows.
- A script's basename when the change is to that script, e.g. `build.sh:`,
  `update.sh:`, `install-task.sh:`.
- `devcontainer` — the build image and its pins.
- `schema` — `schema.json` / `schema-taskrc.json` when changed on their own.
- `doc` — documentation (`README.md`, `docs/`). `tests` — test-only changes,
  including `testdata/` fixtures. `rust` — cross-cutting language/toolchain or
  dependency updates.

For a change spanning two or three components, list them comma-separated
(`build.sh, lint.sh: …`); for more, use `all:` or pick the dominant scope.

## When to add a body

Add a body if any apply: a non-trivial behavior change or refactor; a subtle bug
whose root cause isn't obvious from the diff (note the failure mode and how the
fix addresses it); a performance fix where the measurement matters; a security
fix (note risk/impact); a deliberate divergence from Go Task v3 behaviour (say
what upstream does and why we differ); or a decision involving trade-offs a
reviewer needs to understand.

Anything that changes the fingerprint checksum, and so invalidates existing
`.task` caches, must say so in the body — that is the loudest thing a reviewer
needs to see.

Body content rules:

- **Self-contained.** A reader must understand the change without following
  links.
- **Faithful to the diff.** Every behavior or mechanism described must be
  verifiable in the actual changes; do not reference code that isn't there.
- **High-level.** Explain *what* changed and *why*. Do not dive into technical
  detail unless it is strictly necessary to understand the change — fine-grained
  mechanics (algorithm choice, data-structure invariants, line-level rationale)
  belong in code comments next to the code, not the commit body. If the body
  reads like a file-by-file walkthrough of the diff, it is too detailed.
- **Lead with the change, not the incident.** State what the commit does; give
  the broken state at most one clause of setup. A body that opens by narrating
  the old behavior and then argues its way to the fix is a post-mortem, and
  post-mortems belong in an issue, not the git log.

When in doubt, ship the shorter message.

## Changelog

A commit that changes user-visible behavior — a feature, a fix, a change in how
`task` is invoked or behaves, a new Taskfile field — must update `CHANGELOG.md`
in the same commit, under an `## Unreleased` heading at the top (create it if
absent). At release time a `## v<version>` heading goes in below it —
`taskcore`'s `version_matches_changelog` test refuses a workspace version that
does not match the newest `## v<version>` heading. Purely internal changes
(refactors, tests, CI, build tooling, docs) do not get an entry.

Changelog entries are pitched one level higher than the commit message: describe
what a user gains or what now behaves differently, in their terms. No internal
mechanisms, module or crate names, or implementation detail — if a sentence only
makes sense to someone reading the source, it belongs in the commit message or a
code comment, not the changelog. Call out anything that diverges from Go Task v3
or invalidates existing caches, since that is what a user upgrading needs to
know.

## Diff → verb cues

| Change pattern | Verb |
|----------------|------|
| Added file(s) / capability | `add` |
| Removed file / code | `remove` / `drop` |
| Modified logic path | `update` / `adjust` |
| Fixing a bug | `fix` |
| Performance work | `optimize` / `speed up` |
| Behavior-preserving restructure | `refactor` |
| Dependency / toolchain version | `bump` / `upgrade` |
| Tests only | `tests:` scope |
| Docs only | `doc:` scope |

## Don't

- No "WIP" / "work in progress" commits on shared branches.
- No redundant phrasing: "fix", not "fix bug in"; drop "update code".
- No `Co-Authored-By` or `Signed-off-by` trailers, and no AI-assistant
  attribution.
- Do not write a message for an empty, whitespace-only, or
  lockfile-only-with-no-source diff — there is nothing to commit.

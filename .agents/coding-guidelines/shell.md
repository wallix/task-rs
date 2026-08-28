# Shell Coding Guidelines

Applies to: `*.sh` (the build, package, release-notes, lint, format, audit,
update, and installer scripts at the repo root).

See [`../coding-guidelines.md`](../coding-guidelines.md) for general conventions
and formatting requirements that apply to all code.

## Conventions

- Bash with `set -euo pipefail` and `cd "$(dirname "$0")"` at the top, so the
  script is location-independent and fails loudly. `install-task.sh` is the
  exception: `#!/bin/sh` with `set -e` and no `cd`, so it runs from wherever it
  is invoked.
- **POSIX-compatible when the script must run outside a Bash host** —
  `install-task.sh` is fetched and run on arbitrary machines, and anything
  invoked via `sh` inside the Alpine devcontainer image has no `bash`. Avoid
  bashisms in those and verify with `sh -n`.
- Keep the two container backends symmetric: scripts that build or check inside
  the devcontainer prefer a `vk` on `PATH` and fall back to Docker, with
  `--docker` forcing Docker. Both paths must pass identical flags so they
  produce identical results.
- Preserve current flag semantics. New flags get a clear `--long-name` and a
  usage line. `build.sh` owns its whole flag set and rejects unknown args with
  `exit 2`; the cargo wrappers (`lint.sh`, `fmt.sh`, `audit.sh`) deliberately
  forward extra args to the underlying command (`./fmt.sh --check`) — preserve
  that.
- Make destructive or expensive operations safe: verbose output, idempotent
  re-runs (`update.sh` is a no-op when already current), and preconditions
  checked up front rather than half way through. Clean temporary directories on
  every exit path with `trap ... EXIT` as `package.sh` does.
- Keep builds reproducible: pin inputs (toolchain channel, base image tag
  **and** digest, apk versions) and neutralize timestamps and host paths. Do not
  float a version that was previously pinned. `build.sh` writes
  `dist/task.sha256` — a rebuild from the same commit must match it.
- Do not introduce `curl` to new external domains without rationale. Verify
  downloads against a pinned checksum.
- Never embed credentials in scripts or Docker layers, and do not echo secrets
  (registry tokens, `$TASK_VK_LOCK_TOKEN`).
- Prefer the dedicated tools the repo already uses (`sed -nE`, `awk`,
  `sha256sum`, `cargo`) over reinventing parsing; keep one script focused on one
  job (build vs. lint vs. fmt vs. audit vs. update).

#!/usr/bin/env bash
# audit.sh — scan the dependency tree for known RUSTSEC advisories against the committed
# Cargo.lock (reading the ignore list in .cargo/audit.toml if present). Extra arguments are
# forwarded to cargo-audit (e.g. --deny warnings).
#
# Backend: prefers a `vk` on PATH (the microVM builder, like build.sh/lint.sh), else Docker
# — both run in the devcontainer where cargo-audit is baked in. Pass --docker to force the
# Docker backend.
set -euo pipefail
cd "$(dirname "$0")"

FORCE_DOCKER=""
args=()
for arg in "$@"; do
  case "$arg" in
    --docker) FORCE_DOCKER=1 ;;
    *) args+=("$arg") ;;
  esac
done

if [ -z "$FORCE_DOCKER" ] && command -v vk >/dev/null 2>&1; then
  # Dogfood the vk on PATH: it builds the devcontainer image and runs the audit in a microVM
  # with the repo mounted at the workdir. --net gives cargo-audit egress to fetch the RustSec
  # advisory database.
  echo "audit.sh: auditing with vk from PATH ($(command -v vk)); pass --docker to force Docker" >&2
  exec vk run \
    --file .devcontainer/Dockerfile --context .devcontainer \
    --workdir "$PWD" --net \
    -- cargo audit "${args[@]}"
fi

docker build -t task-build -f .devcontainer/Dockerfile .devcontainer

exec docker run --rm \
  --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$PWD":/work -w /work \
  task-build \
  cargo audit "${args[@]}"

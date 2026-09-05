#!/usr/bin/env bash
# Run clippy on all targets in the devcontainer's locked Nix toolchain. ./update.sh
# keeps it aligned with rust-toolchain.toml. Like build.sh, prefer vk on PATH
# (microVM); --docker forces Docker. Forward extra arguments to clippy (e.g. --fix).
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
  # Dogfood the vk on PATH: it builds the devcontainer image and runs clippy in a microVM
  # with the repo mounted at the workdir. --net gives cargo egress to fetch crates; the
  # workspace compile wants every CPU and enough RAM not to OOM rustc.
  echo "lint.sh: linting with vk from PATH ($(command -v vk)); pass --docker to force Docker" >&2
  exec vk run \
    --file .devcontainer/Dockerfile --context .devcontainer --target task-build \
    --workdir "$PWD" --net --cpus host --mem 8G \
    -- cargo clippy --workspace --all-targets --locked -- -D warnings "${args[@]}"
fi

docker build --target task-build -t task-build -f .devcontainer/Dockerfile .devcontainer

docker run --rm \
  --user "$(id -u):$(id -g)" -e HOME=/tmp -e CARGO_HOME=/work/target/.cargo-home \
  -v "$PWD":/work -w /work \
  task-build \
  cargo clippy --workspace --all-targets --locked -- -D warnings "${args[@]}"

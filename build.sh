#!/usr/bin/env bash
# Build the production static-musl `task` binary.
#
# Two backends produce the same artifact with the same pinned toolchain:
#   - default: dogfood a `vk` on PATH — build the devcontainer Dockerfile into a
#     microVM and compile the repo (mounted at the workdir) inside it.
#   - --docker: `docker build` the devcontainer image, then `docker run` the compile.
# Both mount the repo, pass identical flags, and target x86_64-unknown-linux-musl,
# so the bytes match either way. Output: ./dist/task — a stripped static ELF, plus
# a dist/task.sha256 manifest (rebuild from the same commit to confirm).
set -euo pipefail
cd "$(dirname "$0")"

IMAGE=task-build
TARGET=x86_64-unknown-linux-musl
OUT=dist

FORCE_DOCKER=""
for arg in "$@"; do
  case "$arg" in
    --docker) FORCE_DOCKER=1 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

# Path-independence for reproducible builds: remap the mounted /work to stable
# names in both the Rust debug info and the vendored C (-ffile-prefix-map).
RUSTFLAGS_VAL="--remap-path-prefix=/work=/src --remap-path-prefix=/work/target/.cargo-home=/cargo"
BUILD_ENV=(
  HOME=/tmp
  CARGO_HOME=/work/target/.cargo-home
  CARGO_TARGET_DIR=/work/target
  SOURCE_DATE_EPOCH=0
  "RUSTFLAGS=$RUSTFLAGS_VAL"
  # Alpine's gcc is the musl C compiler; point cc-rs (ring, zstd) at it and make
  # its output path-independent too.
  "CC_x86_64_unknown_linux_musl=gcc"
  "CFLAGS_x86_64_unknown_linux_musl=-ffile-prefix-map=/work=/src -ffile-prefix-map=/work/target/.cargo-home=/cargo"
)
BUILD_CMD="cargo build --release -p task --target $TARGET"

VK_BIN=""
if [ -z "$FORCE_DOCKER" ] && command -v vk >/dev/null 2>&1; then
  VK_BIN=$(command -v vk)
  echo "build.sh: building with vk from PATH ($VK_BIN); pass --docker to force the Docker backend" >&2
fi

if [ -n "$VK_BIN" ]; then
  # ---- dogfood backend: vk microVM ----
  # The devcontainer RUN steps need egress for apk, and the compile needs egress for
  # cargo (--net); the workspace build wants all CPUs and enough RAM not to OOM rustc.
  exports=""
  for e in "${BUILD_ENV[@]}"; do exports+="export ${e%%=*}='${e#*=}'; "; done
  "$VK_BIN" run \
    --file .devcontainer/Dockerfile --context .devcontainer \
    --workdir "$PWD" --net --cpus host --mem 8G \
    -- sh -c "${exports}${BUILD_CMD}"
else
  # ---- default backend: Docker ----
  docker build -t "$IMAGE" -f .devcontainer/Dockerfile .devcontainer
  # Build as the host user so target/ stays writable and no root-owned files leak out.
  docker_env=()
  for e in "${BUILD_ENV[@]}"; do docker_env+=(-e "$e"); done
  docker run --rm \
    --user "$(id -u):$(id -g)" \
    "${docker_env[@]}" \
    -v "$PWD":/work -w /work \
    "$IMAGE" \
    sh -c "$BUILD_CMD"
fi

mkdir -p "$OUT"
# Replace atomically (temp + rename): rename never hits "Text file busy" if the old
# binary is still executing.
cp "target/$TARGET/release/task" "$OUT/.task.tmp"
mv -f "$OUT/.task.tmp" "$OUT/task"

# Reproducible-build manifest: the binary is byte-for-byte determined by the
# source at a given commit, so record the commit and the verify recipe next to
# the checksum. Rebuilding the same commit through build.sh must reproduce it.
commit=$(git -C "$(dirname "$0")" rev-parse HEAD 2>/dev/null || echo unknown)
dirty=""
git -C "$(dirname "$0")" diff --quiet 2>/dev/null || dirty=" (dirty tree)"
(
  cd "$OUT"
  {
    echo "# task reproducible build manifest"
    echo "# commit: ${commit}${dirty}"
    echo "# verify: git checkout ${commit} && ./build.sh && sha256sum -c dist/task.sha256"
    sha256sum task
  } > task.sha256
)
echo "build.sh: wrote $OUT/task" >&2
file "$OUT/task" >&2 || true

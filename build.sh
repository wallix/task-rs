#!/usr/bin/env bash
# Build the production static-musl `task` binary.
#
# Uses vk when available, or Docker with --docker. Both run the pinned
# devcontainer image and write dist/task plus dist/task.sha256.
#
# Flags:
#   --docker             force the Docker backend
#   --target=<arch>      the musl target to build: x86_64, aarch64, or either full
#                        triple. Defaults to the host architecture; cross-builds
#                        are rejected.
set -euo pipefail
cd "$(dirname "$0")"

STAGE=task-build # Dockerfile stage and local image tag
OUT=dist

FORCE_DOCKER=""
REQ_TARGET=""
for arg in "$@"; do
  case "$arg" in
    --docker) FORCE_DOCKER=1 ;;
    # Reject an explicitly empty target instead of using the host default.
    --target=) echo "build.sh: --target needs a value (x86_64 or aarch64)" >&2; exit 2 ;;
    --target=*) REQ_TARGET="${arg#*=}" ;;
    *) echo "build.sh: unknown argument: $arg (--docker, --target=<arch>)" >&2; exit 2 ;;
  esac
done

# --target asserts the expected host architecture; it does not cross-compile.
case "$(uname -m)" in
  x86_64|amd64) HOST_ARCH=x86_64 ;;
  aarch64|arm64) HOST_ARCH=aarch64 ;;
  *) echo "build.sh: no musl release target for $(uname -m)" >&2; exit 2 ;;
esac
ARCH="$HOST_ARCH"
if [ -n "$REQ_TARGET" ]; then
  case "$REQ_TARGET" in
    x86_64|amd64|x86_64-unknown-linux-musl) ARCH=x86_64 ;;
    aarch64|arm64|aarch64-unknown-linux-musl) ARCH=aarch64 ;;
    *) echo "build.sh: --target must be x86_64 or aarch64 (or either musl triple), not '$REQ_TARGET'" >&2; exit 2 ;;
  esac
  if [ "$ARCH" != "$HOST_ARCH" ]; then
    echo "build.sh: --target=$REQ_TARGET on a $HOST_ARCH host — the musl build is native, so run it on a $ARCH machine" >&2
    exit 2
  fi
fi
TARGET="$ARCH-unknown-linux-musl"
# Override DOCKER_DEFAULT_PLATFORM to select the matching image architecture.
case "$ARCH" in
  x86_64) PLATFORM=linux/amd64 ;;
  aarch64) PLATFORM=linux/arm64 ;;
esac

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
  "CC_${TARGET//-/_}=gcc"
  "CFLAGS_${TARGET//-/_}=-ffile-prefix-map=/work=/src -ffile-prefix-map=/work/target/.cargo-home=/cargo"
)
BUILD_CMD="cargo build --release -p task --target $TARGET"

# Read and validate the pinned inputs recorded in the build manifest.
toolchain=$(sed -nE 's/^channel = "(.*)"$/\1/p' rust-toolchain.toml)
base_image=$(sed -nE 's/^FROM (rust:[^ ]+).*$/\1/p' .devcontainer/Dockerfile)
apk_pins=$(sha256sum .devcontainer/apk-pins.txt | cut -d' ' -f1)
: "${toolchain:?no channel found in rust-toolchain.toml}"
: "${base_image:?no FROM rust: line found in .devcontainer/Dockerfile}"

VK_BIN=""
if [ -z "$FORCE_DOCKER" ] && command -v vk >/dev/null 2>&1; then
  VK_BIN=$(command -v vk)
  echo "build.sh: building with vk from PATH ($VK_BIN); pass --docker to force the Docker backend" >&2
fi

if [ -n "$VK_BIN" ]; then
  # ---- dogfood backend: vk microVM ----
  # The devcontainer RUN steps need egress for apk, and the compile needs egress for
  # cargo (--net); the workspace build wants all CPUs and enough RAM not to OOM rustc.
  # This --target selects the Dockerfile stage, not a Rust target.
  exports=""
  for e in "${BUILD_ENV[@]}"; do exports+="export ${e%%=*}='${e#*=}'; "; done
  "$VK_BIN" run \
    --file .devcontainer/Dockerfile --context .devcontainer --target "$STAGE" \
    --workdir "$PWD" --net --cpus host --mem 8G \
    -- sh -c "${exports}${BUILD_CMD}"
else
  # ---- default backend: Docker ----
  docker build --platform "$PLATFORM" --target "$STAGE" -t "$STAGE" \
    -f .devcontainer/Dockerfile .devcontainer
  # Build as the host user so target/ stays writable and no root-owned files leak out.
  docker_env=()
  for e in "${BUILD_ENV[@]}"; do docker_env+=(-e "$e"); done
  docker run --rm \
    --platform "$PLATFORM" \
    --user "$(id -u):$(id -g)" \
    "${docker_env[@]}" \
    -v "$PWD":/work -w /work \
    "$STAGE" \
    sh -c "$BUILD_CMD"
fi

mkdir -p "$OUT"
# Replace atomically (temp + rename): rename never hits "Text file busy" if the old
# binary is still executing.
cp "target/$TARGET/release/task" "$OUT/.task.tmp"
mv -f "$OUT/.task.tmp" "$OUT/task"

# Record the source and pinned inputs beside the binary checksum.
commit=$(git rev-parse HEAD 2>/dev/null || echo unknown)
dirty=""
# --quiet HEAD, not bare --quiet: a staged-but-uncommitted edit is a dirty tree too.
git diff --quiet HEAD 2>/dev/null || dirty=" (dirty tree)"
(
  cd "$OUT"
  {
    echo "# task reproducible build manifest"
    echo "# commit:     ${commit}${dirty}"
    echo "# target:     ${TARGET}"
    echo "# toolchain:  ${toolchain}"
    echo "# base image: ${base_image}"
    echo "# apk pins:   sha256:${apk_pins}"
    # Preserve the expected manifest because build.sh overwrites task.sha256.
    echo "# verify: git checkout ${commit} && cp dist/task.sha256 /tmp/task.expected &&"
    echo "#         ./build.sh && ( cd dist && sha256sum -c /tmp/task.expected )"
    sha256sum task
  } > task.sha256
)
echo "build.sh: wrote $OUT/task" >&2
file "$OUT/task" >&2 || true

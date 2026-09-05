#!/usr/bin/env bash
# Bump the pinned Rust toolchain to the latest stable release and re-pin the build
# inputs.
#
# Update the channel in rust-toolchain.toml and the Nix flake, the nixos/nix image
# tag and digest in the Dockerfile, and the nixpkgs/rust-overlay commits in
# .devcontainer/nix/flake.lock.
# cargo-audit moves with the locked closure rather than a separate version pin.
#
# Unlike the apk-pinning version, this can change files even when Rust and the base
# image are current: `nix flake update` resolves both input branches again and
# rewrites flake.lock when either moves. Review the diff, then run ./build.sh.
#
# Requires rustup, curl and docker with buildx. Nix runs inside a container;
# the host does not need it.
set -euo pipefail
cd "$(dirname "$0")"

# Check tools before rewriting pins so a missing tool cannot leave partial edits.
for t in rustup curl docker; do
  command -v "$t" >/dev/null || { echo >&2 "ERROR: update.sh needs $t on PATH"; exit 1; }
done
docker buildx version >/dev/null 2>&1 ||
  { echo >&2 "ERROR: update.sh needs the docker buildx plugin (manifest-list digest lookup)"; exit 1; }

# Ask the local rustup for the latest stable version rather than scraping
# release pages.
rustup update stable
LATEST=$(rustup run stable rustc --version | awk '{print $2}')
echo "latest stable: $LATEST"

sed -i -E "s/^channel = \".*\"/channel = \"$LATEST\"/" rust-toolchain.toml
# The flake pins the channel inline: it lives under .devcontainer/nix/ (inside the Docker
# build context) and a flake cannot read `../../rust-toolchain.toml` in pure eval.
sed -i -E "s/(rust-bin\.stable\.\")[^\"]+(\")/\1${LATEST}\2/" .devcontainer/nix/flake.nix
sed -i -E "s/(# Exact toolchain: channel )[0-9]+\.[0-9]+\.[0-9]+/\1${LATEST}/" .devcontainer/nix/flake.nix
# Check both pins as AGENTS.md requires: sed can miss reformatted Nix, and the
# `nix build` below would still accept the old channel.
assert_pinned() { # <file> <extended-regex>
  grep -qE "$2" "$1" || { echo >&2 "ERROR: update.sh could not set the pin in $1 (no match for /$2/)"; exit 1; }
}
assert_pinned rust-toolchain.toml "^channel = \"${LATEST}\"$"
assert_pinned .devcontainer/nix/flake.nix "rust-bin\.stable\.\"${LATEST}\""
assert_pinned .devcontainer/nix/flake.nix "# Exact toolchain: channel ${LATEST}[^0-9]"

# Re-pin the nixos/nix base image to its newest release, by manifest-list digest so the
# FROM line stays pinned and still resolves to each release runner's architecture. Docker
# Hub's tag API is the only published index of nixos/nix releases — the image carries no
# "latest semver" tag — so the version is read from there and the digest from the registry
# itself. The listing is the 100 most recently updated tags: a release older than that
# window, or a response reformat, yields nothing or something stale, so the result is
# checked to be a version and to not go backwards from the pin in the Dockerfile.
NIX_VER=${NIX_VER:-$(
  curl -fsSL 'https://hub.docker.com/v2/repositories/nixos/nix/tags?page_size=100&ordering=last_updated' \
    | tr ',' '\n' | sed -nE 's/^"name":"([0-9]+\.[0-9]+\.[0-9]+)"$/\1/p' \
    | sort -t. -k1,1n -k2,2n -k3,3n | tail -1
)}
case "$NIX_VER" in
  [0-9]*) ;;
  *) echo >&2 "ERROR: could not resolve the latest nixos/nix version (got '$NIX_VER')"; exit 1 ;;
esac
CUR_NIX_VER=$(sed -nE 's/^FROM nixos\/nix:([0-9]+\.[0-9]+\.[0-9]+).*/\1/p' .devcontainer/Dockerfile)
: "${CUR_NIX_VER:?no FROM nixos/nix:<version> line found in .devcontainer/Dockerfile}"
if [ "$NIX_VER" != "$CUR_NIX_VER" ] &&
   [ "$(printf '%s\n%s\n' "$NIX_VER" "$CUR_NIX_VER" | sort -t. -k1,1n -k2,2n -k3,3n | head -1)" != "$CUR_NIX_VER" ]; then
  echo >&2 "ERROR: resolved nixos/nix $NIX_VER is older than the pinned $CUR_NIX_VER — stale tag listing?"
  echo >&2 "       set NIX_VER=<version> to override."
  exit 1
fi
IMG="nixos/nix:${NIX_VER}"
DIGEST=$(docker buildx imagetools inspect "$IMG" | sed -nE 's/^Digest:[[:space:]]+(sha256:.*)$/\1/p')
case "$DIGEST" in
  sha256:*) ;;
  *) echo >&2 "ERROR: could not resolve the manifest-list digest of $IMG (got '$DIGEST')"; exit 1 ;;
esac
sed -i -E "s#^FROM nixos/nix:[^ ]*#FROM ${IMG}@${DIGEST}#" .devcontainer/Dockerfile
sed -i -E "s/^# nixos\/nix [0-9]+\.[0-9]+\.[0-9]+ by/# nixos\/nix ${NIX_VER} by/" .devcontainer/Dockerfile
assert_pinned .devcontainer/Dockerfile "^FROM ${IMG}@${DIGEST} "
assert_pinned .devcontainer/Dockerfile "^# nixos/nix ${NIX_VER} by"

# Refresh the toolchain's archival lock and evaluate the closure with the new
# channel, catching Rust releases not yet in rust-overlay before a release build.
# Both input branches are resolved again (see above). Nix runs as root to write
# the base image's store; return flake.lock to the invoking user before exit.
echo "refreshing .devcontainer/nix/flake.lock in ${IMG}@${DIGEST} ..."
docker run --rm \
  -e NIX_CONFIG='experimental-features = nix-command flakes
sandbox = false' \
  -v "$PWD/.devcontainer/nix":/src/nix -w /src/nix \
  "${IMG}@${DIGEST}" \
  sh -ec "
    nix flake update --flake path:/src/nix
    nix build --dry-run 'path:/src/nix#buildEnv'
    chown $(id -u):$(id -g) flake.lock
  "

echo "updated:"
grep -E '^channel' rust-toolchain.toml
grep -E 'rust-bin\.stable\.' .devcontainer/nix/flake.nix
grep -E '^FROM nixos/nix:' .devcontainer/Dockerfile
echo "flake inputs (.devcontainer/nix/flake.lock):"
grep -E '"(owner|repo|rev)":' .devcontainer/nix/flake.lock | tr -d ' ",' | sed 's/^/  /'

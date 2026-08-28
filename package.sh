#!/usr/bin/env bash
# Build a deterministic release archive for a compiled task binary.
#
# The archive contains the binary at its root, plus README.md, LICENSE, and shell
# completions. Outputs:
#
#   dist/task-<platform>.tar.gz   (.zip for windows-*)
#   dist/task-<platform>.sha256   one sha256sum line naming that archive
#
# Member order, ownership, modes, and timestamps are fixed. All platforms are
# packaged on Linux to use the same GNU tar/gzip or Info-ZIP implementation.
#
# Usage:
#   ./package.sh --platform <name> --binary <path> [--out <dir>]
#
# Flags also accept --flag=value. Paths are relative to the repository root.
#
#   --platform  one of the release matrix names: linux-x86_64, linux-aarch64,
#               macos-x86_64, macos-aarch64, windows-x86_64, windows-aarch64
#   --binary    the compiled binary to package
#   --out       directory to write the archive and sidecar to (default: dist)
#
# Archive bytes depend on the archiver. dist/task.sha256 covers only the binary.
set -euo pipefail
cd "$(dirname "$0")"

# Earliest timestamp supported by ZIP: 1980-01-01T00:00:00Z.
EPOCH=315532800
PLATFORMS="linux-x86_64 linux-aarch64 macos-x86_64 macos-aarch64 windows-x86_64 windows-aarch64"

PLATFORM=""
BINARY=""
OUT=dist
# Report missing option values before shift fails under set -e.
need_val() { [ "$2" -ge 2 ] || { echo "package.sh: $1 needs a value" >&2; exit 2; }; }
while [ $# -gt 0 ]; do
  case "$1" in
    # Reject empty values before the =* cases.
    --platform=|--binary=|--out=) echo "package.sh: ${1%=} needs a value" >&2; exit 2 ;;
    --platform) need_val "$1" $#; PLATFORM="$2"; shift 2 ;;
    --platform=*) PLATFORM="${1#*=}"; shift ;;
    --binary) need_val "$1" $#; BINARY="$2"; shift 2 ;;
    --binary=*) BINARY="${1#*=}"; shift ;;
    --out) need_val "$1" $#; OUT="$2"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    *) echo "package.sh: unknown argument: $1 (--platform, --binary, --out)" >&2; exit 2 ;;
  esac
done

# Validate inputs before creating output.
[ -n "$PLATFORM" ] || { echo "package.sh: --platform is required (one of: $PLATFORMS)" >&2; exit 2; }
[ -n "$BINARY" ] || { echo "package.sh: --binary is required" >&2; exit 2; }
case " $PLATFORMS " in
  *" $PLATFORM "*) ;;
  *) echo "package.sh: unknown platform '$PLATFORM' (one of: $PLATFORMS)" >&2; exit 2 ;;
esac
[ -f "$BINARY" ] || { echo "package.sh: no binary at $BINARY" >&2; exit 1; }
# Check archivers before replacing an existing archive.
case "$PLATFORM" in
  windows-*) tools="zip unzip" ;;
  *) tools="tar gzip" ;;
esac
for t in $tools sha256sum; do
  command -v "$t" >/dev/null || { echo "package.sh: $t is required to package $PLATFORM" >&2; exit 1; }
done

case "$PLATFORM" in
  windows-*) BIN_NAME=task.exe; EXT=zip ;;
  *)         BIN_NAME=task;     EXT=tar.gz ;;
esac
STEM="task-$PLATFORM"
ARCHIVE="$STEM.$EXT"
SIDECAR="$STEM.sha256"

# Stage only the files included in the release archive.
STAGE=$(mktemp -d)
TMP_ARCHIVE=""
TMP_SIDECAR=""
trap 'rm -rf "$STAGE"; rm -f "$TMP_ARCHIVE" "$TMP_SIDECAR"' EXIT
install -m 0755 "$BINARY" "$STAGE/$BIN_NAME"
install -m 0644 README.md LICENSE "$STAGE/"
mkdir -m 0755 "$STAGE/completion"
install -m 0644 crates/task/completion/* "$STAGE/completion/"
# Directory timestamps are stored in the archive.
find "$STAGE" -exec touch -h -d "@$EPOCH" {} +

mkdir -p "$OUT"
OUT_ABS=$(cd "$OUT" && pwd)
# List members explicitly in locale-independent order. --no-recursion prevents
# tar from adding completion files twice.
mapfile -t completions < <(cd "$STAGE" && LC_ALL=C ls completion)
MEMBERS=(LICENSE README.md completion)
for f in "${completions[@]}"; do MEMBERS+=("completion/$f"); done
MEMBERS+=("$BIN_NAME")
# Publish the archive and sidecar only after both are complete.
TMP_ARCHIVE="$OUT_ABS/.$ARCHIVE.tmp"
TMP_SIDECAR="$OUT_ABS/.$SIDECAR.tmp"
if [ "$EXT" = zip ]; then
  # -X omits uid, gid, and high-resolution timestamps.
  rm -f "$TMP_ARCHIVE"
  ( cd "$STAGE" && TZ=UTC zip -qX9 "$TMP_ARCHIVE" "${MEMBERS[@]}" )
else
  # gzip -n leaves out the name and mtime a .gz header would otherwise carry.
  tar --format=gnu --no-recursion --owner=0 --group=0 --numeric-owner \
    --mtime="@$EPOCH" -cf - -C "$STAGE" "${MEMBERS[@]}" \
    | gzip -9n > "$TMP_ARCHIVE"
fi

# task --update requires the binary at the archive root.
if [ "$EXT" = zip ]; then
  members=$(unzip -Z1 "$TMP_ARCHIVE")
else
  members=$(tar tzf "$TMP_ARCHIVE")
fi
grep -qxF "$BIN_NAME" <<<"$members" || {
  echo "package.sh: $ARCHIVE has no $BIN_NAME at its root:" >&2
  echo "$members" >&2
  exit 1
}
# The sidecar uses the bare archive name for sha256sum -c compatibility.
digest=$(sha256sum "$TMP_ARCHIVE" | cut -d' ' -f1)
printf '%s  %s\n' "$digest" "$ARCHIVE" > "$TMP_SIDECAR"
mv -f "$TMP_ARCHIVE" "$OUT_ABS/$ARCHIVE"
mv -f "$TMP_SIDECAR" "$OUT_ABS/$SIDECAR"
echo "package.sh: wrote $OUT/$ARCHIVE" >&2
cat "$OUT_ABS/$SIDECAR" >&2

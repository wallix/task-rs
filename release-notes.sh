#!/usr/bin/env bash
# Print a release tag's CHANGELOG.md section.
#
# Usage: ./release-notes.sh v4.2.0 [> notes.md]
# The matching heading is `## v<X.Y.Z> - <date>`. Output continues until the
# next level-two heading. Missing or empty sections are errors.
set -euo pipefail
cd "$(dirname "$0")"

TAG="${1:-}"
[ -n "$TAG" ] || { echo "release-notes.sh: a release tag is required (v<X.Y.Z>)" >&2; exit 2; }
# Require output paths to be supplied through shell redirection.
[ "$#" -eq 1 ] || { echo "release-notes.sh: expected one argument, the tag" >&2; exit 2; }
case "$TAG" in
  v[0-9]*) ;;
  *) echo "release-notes.sh: '$TAG' is not a v-prefixed release tag" >&2; exit 2 ;;
esac
[ -f CHANGELOG.md ] || { echo "release-notes.sh: CHANGELOG.md not found" >&2; exit 1; }

# Buffer blank lines so trailing whitespace is omitted in one awk pass.
notes=$(awk -v tag="$TAG" '
  /^## / {
    split($0, f, " ")
    inside = (f[2] == tag)
    next
  }
  !inside { next }
  /^[[:space:]]*$/ { if (seen) pending = pending "\n"; next }
  { printf "%s%s\n", pending, $0; pending = ""; seen = 1 }
' CHANGELOG.md)

if [ -z "$notes" ]; then
  echo "release-notes.sh: no '## $TAG' section with a body in CHANGELOG.md" >&2
  exit 1
fi
printf '%s\n' "$notes"

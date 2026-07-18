#!/usr/bin/env bash
# Cut a release: set the version across the workspace, verify the CHANGELOG has
# a matching section, then commit, tag `v<version>`, and push.
#
# Usage: ./release.sh <MAJOR.MINOR.PATCH>
# Add the CHANGELOG entry (a `## v<version>` heading) before running.
set -euo pipefail
cd "$(dirname "$0")"

version="${1:-}"
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  echo "release.sh: expected a MAJOR.MINOR.PATCH version, got '${version}'" >&2
  exit 2
fi
tag="v${version}"

# Preconditions.
if [ -n "$(git status --porcelain)" ]; then
  echo "release.sh: working tree is not clean" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/${tag}" >/dev/null; then
  echo "release.sh: tag ${tag} already exists" >&2
  exit 1
fi
if ! grep -qE "^## v${version}( |\$)" CHANGELOG.md; then
  echo "release.sh: add a '## v${version}' section to CHANGELOG.md first" >&2
  exit 1
fi

# Update the version sources. The workspace version is the only top-level
# `version = "..."` line (dependency versions are inline in `{ ... }`).
printf '%s\n' "${version}" > crates/taskcore/src/version.txt
sed -i -E "s/^version = \"[^\"]*\"/version = \"${version}\"/" Cargo.toml

# Refresh Cargo.lock and confirm the version invariant (version.txt matches the
# CHANGELOG heading) before committing.
cargo build -q -p task
cargo test -q -p taskcore version::

git add crates/taskcore/src/version.txt Cargo.toml Cargo.lock CHANGELOG.md
git commit -m "${tag}"
git tag -a "${tag}" -m "${tag}"

echo "release.sh: committed and tagged ${tag}; pushing" >&2
git push
git push origin "${tag}"
echo "release.sh: pushed ${tag}. The Release workflow will build and publish the artifacts." >&2

#!/usr/bin/env bash
# Release end-to-end gate, run against the packaged release artifacts.
#
# release.yml runs this on the archives build.yml produced and the publish job releases, so
# what is tested is what ships. Nothing is compiled here: the checks unpack the archives and
# drive the shipped `task` binary the way a user would.
#
# The sidecar and version checks come first and are preconditions — a mismatch means these
# are not the built artifacts, or that `task --update` would refuse the release it came
# from, and nothing below would be testing what ships. The rest each report and do not stop
# the run; the exit status is non-zero if any of them failed.
#
#   tests/release-e2e.sh                                  # ./dist, version not compared
#   RELEASE_TAG=v4.5.0 tests/release-e2e.sh               # what release.yml runs
#   DIST=/tmp/assets RELEASE_TAG=v4.5.0 tests/release-e2e.sh
#
# Needs: tar, gzip, unzip, sha256sum, file, cmp, and a Linux archive for the host
# architecture (the host-arch binary is the one actually executed; the other platforms'
# archives are checked structurally). RELEASE_TAG is required under CI — a gated release
# must compare the binary's version against the tag it will be published as.
set -euo pipefail

usage() {
  echo "usage: [DIST=<dir>] [RELEASE_TAG=vX.Y.Z] $0" >&2
  exit 2
}
[ "$#" -eq 0 ] || usage

cd "$(dirname "$0")/.."
REPO=$PWD
DIST=${DIST:-dist}
[ -d "$DIST" ] || { echo "release-e2e: no artifact directory at $DIST" >&2; exit 2; }
DIST=$(cd "$DIST" && pwd)

for t in tar gzip unzip sha256sum file cmp; do
  command -v "$t" >/dev/null || { echo "release-e2e: $t is required" >&2; exit 2; }
done

case "$(uname -m)" in
  x86_64 | amd64) HOST_ARCH=x86_64 ;;
  aarch64 | arm64) HOST_ARCH=aarch64 ;;
  *) echo "release-e2e: no linux release archive for $(uname -m)" >&2; exit 2 ;;
esac
HOST_ARCHIVE="$DIST/task-linux-$HOST_ARCH.tar.gz"

# Every platform the release publishes, so a missing archive fails the gate rather than
# going unnoticed. Keep in sync with package.sh.
PLATFORMS="linux-x86_64 linux-aarch64 macos-x86_64 macos-aarch64 windows-x86_64 windows-aarch64"

echo "release-e2e: testing the artifacts in $DIST"

########################################################## preconditions

echo
echo "################ sha256 sidecars"
# The bytes under test are the bytes the sidecars vouch for. Every archive's sidecar is
# checked, and the host archive's must exist — package.sh always writes it, so without it
# these are not the built artifacts.
shopt -s nullglob
sidecars=("$DIST"/task-*.sha256)
shopt -u nullglob
[ "${#sidecars[@]}" -gt 0 ] || { echo "release-e2e: no task-*.sha256 in $DIST" >&2; exit 1; }
( cd "$DIST" && sha256sum -c ./task-*.sha256 ) || exit 1

echo
echo "################ archives"
# Under CI every published platform must be present: a missing archive means a build job
# produced nothing and the release would ship short. Locally, check whatever
# `./build.sh --package` has left in dist/.
PRESENT=""
missing=""
for platform in $PLATFORMS; do
  case $platform in
    windows-*) ext=zip ;;
    *) ext=tar.gz ;;
  esac
  if [ -f "$DIST/task-$platform.$ext" ]; then
    PRESENT="$PRESENT $platform"
  else
    missing="$missing $platform"
  fi
done
if [ -n "$missing" ]; then
  if [ -n "${CI:-}" ]; then
    echo "release-e2e: missing release archives:$missing" >&2
    exit 1
  fi
  echo "not present, not checked:$missing"
fi
echo "present:$PRESENT"
[ -f "$HOST_ARCHIVE" ] || { echo "release-e2e: no $HOST_ARCHIVE to run" >&2; exit 1; }

# Unpack the host archive once; every executing check below runs this binary.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/unpacked"
tar -xzf "$HOST_ARCHIVE" -C "$WORK/unpacked" || exit 1
TASK="$WORK/unpacked/task"
[ -x "$TASK" ] || { echo "release-e2e: no executable task in $HOST_ARCHIVE" >&2; exit 1; }

echo
echo "################ version"
out=$("$TASK" --version) || { echo "release-e2e: task --version failed" >&2; exit 1; }
echo "$out"
if [ -n "${RELEASE_TAG:-}" ]; then
  want=${RELEASE_TAG#v}
  # Build metadata is appended as `+<commit>`; only the version itself is load-bearing,
  # because that is what `task --update` compares against a release tag.
  if [ "${out%%+*}" = "$want" ]; then
    echo "matches $RELEASE_TAG"
  else
    echo "release-e2e: the binary is not version $want (tag $RELEASE_TAG)" >&2
    exit 1
  fi
elif [ -n "${CI:-}" ]; then
  echo "release-e2e: RELEASE_TAG unset; a gated release must compare the two" >&2
  exit 2
else
  echo "RELEASE_TAG unset; not compared"
fi

########################################################## checks

names=()
results=()
failed=0

# Run one named check in a subshell so a failing check cannot leave the rest running in a
# half-set-up directory, and record its verdict instead of aborting the gate.
#
# The subshell must not be the condition of an `if`, or the left side of `||`: bash turns
# errexit OFF inside a command whose status is being tested, so `set -e` in there would be
# inert and a check would report ok on the strength of its last command alone. Disable
# errexit around a standalone subshell instead, and read $? afterwards.
check() {
  local name=$1
  shift
  echo
  echo "################ $name"
  local rc=0
  set +e
  ( set -eo pipefail; "$@" )
  rc=$?
  set -e
  names+=("$name")
  if [ "$rc" -eq 0 ]; then
    results+=(ok)
  else
    results+=(FAILED)
    failed=1
    echo "release-e2e: $name FAILED" >&2
  fi
}

# Every archive carries the binary at its root (what `task --update` replaces itself
# from), the docs, and the four completion scripts — byte-identical to the ones in the
# tree, so a release cannot ship stale completions.
archive_contents() {
  local platform ext bin members expected
  for platform in $PRESENT; do
    case $platform in
      windows-*) ext=zip; bin=task.exe ;;
      *) ext=tar.gz; bin=task ;;
    esac
    if [ "$ext" = zip ]; then
      members=$(unzip -Z1 "$DIST/task-$platform.$ext")
    else
      members=$(tar tzf "$DIST/task-$platform.$ext")
    fi
    expected=$(printf '%s\n' LICENSE README.md completion/ completion/task.bash \
      completion/task.fish completion/task.ps1 completion/task.zsh "$bin" | sort)
    # Both archivers list the directory member as `completion/`; the sed only guards
    # against a bare `completion` so the comparison is of the whole set, not a subset.
    if [ "$(printf '%s\n' "$members" | sed 's#^completion$#completion/#' | sort)" != "$expected" ]; then
      echo "task-$platform.$ext members differ from the expected set:" >&2
      diff <(printf '%s\n' "$expected") \
        <(printf '%s\n' "$members" | sed 's#^completion$#completion/#' | sort) >&2 || true
      return 1
    fi
    echo "task-$platform.$ext: $(printf '%s\n' "$members" | wc -l) members, ok"
  done
  for f in "$REPO"/crates/task/completion/*; do
    cmp "$f" "$WORK/unpacked/completion/$(basename "$f")" || return 1
  done
  echo "completions match crates/task/completion/"
}

# The Linux build manifest is the archiver-independent digest docs/reference/
# reproducible-builds.md tells users to check a rebuild against. It has to describe the
# binary this archive actually carries.
build_manifest() {
  local manifest="$DIST/task-linux-$HOST_ARCH.build-info.txt"
  [ -f "$manifest" ] || { echo "no $manifest" >&2; return 1; }
  grep -q '^# task reproducible build manifest$' "$manifest" \
    || { echo "$manifest is not a build manifest" >&2; return 1; }
  ( cd "$WORK/unpacked" && sha256sum -c <(grep -v '^#' "$manifest") )
  sed -n 's/^# \(commit\|target\|toolchain\|base image\|flake lock\):/\1:/p' "$manifest"
}

# A release binary links no system C library: that is the whole point of the musl-static
# build, and it is what makes one archive run on any glibc or musl distro.
static_binary() {
  local out
  out=$(file -L "$TASK")
  echo "$out"
  # A static PIE — what rustc emits for musl — is reported as `static-pie linked`.
  case $out in
    *"statically linked"* | *"static-pie linked"*) ;;
    *) echo "not a static binary" >&2; return 1 ;;
  esac
  # ldd is the cross-check that no interpreter is actually needed. It exits non-zero on a
  # static binary on some libcs, so read its output rather than its status.
  if command -v ldd >/dev/null; then
    out=$(ldd "$TASK" 2>&1 || true)
    printf '%s\n' "$out" | grep -qE 'not a dynamic executable|statically linked' || {
      printf '%s\n' "$out" >&2
      echo "the binary has dynamic dependencies" >&2
      return 1
    }
  fi
}

# `task --schema` is embedded in the binary and published as schema.json; editors fetch
# one and validate against the other, so they must agree.
embedded_schema() {
  "$TASK" --schema > "$WORK/schema.json"
  cmp "$REPO/schema.json" "$WORK/schema.json"
  echo "--schema matches schema.json ($(wc -c < "$WORK/schema.json") bytes)"
}

# `--completion <shell>` prints from the same embedded copy the archive ships.
embedded_completions() {
  local shell
  for shell in bash zsh fish powershell; do
    "$TASK" --completion "$shell" > "$WORK/completion.out"
    [ -s "$WORK/completion.out" ] || { echo "--completion $shell is empty" >&2; return 1; }
    echo "--completion $shell: $(wc -l < "$WORK/completion.out") lines"
  done
}

# Dogfood: the shipped binary runs this repository's own Taskfile, which is authored in
# the native Jinja dialect. Listing it exercises the reader, the templater and the CLI.
own_taskfile() {
  local out
  out=$("$TASK" --dir "$REPO" --list)
  echo "$out"
  local t
  for t in build test lint fmt release:build; do
    printf '%s\n' "$out" | grep -qE "^\* $t:" || {
      echo "task $t missing from --list of the repo Taskfile" >&2
      return 1
    }
  done
}

# `--init` writes the template embedded in the binary, and the binary then runs it.
init_template() {
  local dir="$WORK/init"
  mkdir -p "$dir"
  ( cd "$dir" && "$TASK" --init )
  [ -f "$dir/Taskfile.yml" ] || { echo "--init wrote no Taskfile.yml" >&2; return 1; }
  local out
  out=$( cd "$dir" && "$TASK" )
  echo "$out"
  [ "$out" = "Hello, world!" ] || { echo "the --init Taskfile did not greet" >&2; return 1; }
}

# The runner itself, on a Taskfile that uses the features a build system depends on:
# dependencies, static and dynamic (`sh:`) variables, fingerprint-based sources/generates
# caching with its --status report, and the exit code a failing command produces.
runner_features() {
  local dir="$WORK/run"
  mkdir -p "$dir"
  # The Go `text/template` dialect, which is what a v3 Taskfile in the wild uses and what
  # parity is measured against; the `own_taskfile` check above covers the native Jinja
  # dialect. Its deprecation notice goes to stderr, so it stays out of the comparisons.
  cat > "$dir/Taskfile.yml" <<'YML'
version: '3'

vars:
  NAME: release
  STAMP:
    sh: echo stamped

tasks:
  default:
    deps: [dep]
    cmds:
      - echo "{{.NAME}} {{.STAMP}}"

  dep:
    cmds:
      - echo dep ran

  build:
    sources: [in.txt]
    generates: [out.txt]
    cmds:
      - cp in.txt out.txt

  boom:
    cmds:
      - exit 7
YML
  cd "$dir"
  local out
  out=$("$TASK" --silent)
  echo "$out"
  [ "$out" = "dep ran
release stamped" ] || { echo "deps, vars or sh: vars did not run as expected" >&2; return 1; }

  echo hello > in.txt
  "$TASK" build
  [ "$(cat out.txt)" = hello ] || { echo "build produced no out.txt" >&2; return 1; }

  # Unchanged sources: the second run is a no-op and --status agrees. --status only
  # reports (it exits 0 either way), so its verdict is the line it prints.
  out=$("$TASK" build 2>&1)
  echo "$out"
  printf '%s\n' "$out" | grep -q 'is up to date' || { echo "build was not cached" >&2; return 1; }
  out=$("$TASK" --status build 2>&1)
  printf '%s\n' "$out" | grep -q '"build" is up to date' || {
    printf '%s\n' "$out" >&2
    echo "--status reports a task it just cached as stale" >&2
    return 1
  }

  # A changed source invalidates the fingerprint.
  echo goodbye > in.txt
  out=$("$TASK" --status build 2>&1)
  printf '%s\n' "$out" | grep -q '"build" is not up to date' || {
    printf '%s\n' "$out" >&2
    echo "--status reports a stale task as current" >&2
    return 1
  }
  "$TASK" build
  [ "$(cat out.txt)" = goodbye ] || { echo "build did not rerun on a changed source" >&2; return 1; }

  # A failing command exits 201 (Go's CodeTaskRunError); -x propagates the command's own
  # status instead. Both are part of the drop-in-compatible surface.
  local rc=0
  "$TASK" boom 2>/dev/null || rc=$?
  [ "$rc" -eq 201 ] || { echo "expected exit 201 from a failing command, got $rc" >&2; return 1; }
  rc=0
  "$TASK" --exit-code boom 2>/dev/null || rc=$?
  [ "$rc" -eq 7 ] || { echo "expected --exit-code to propagate exit 7, got $rc" >&2; return 1; }
  echo "exit statuses: 201 without --exit-code, 7 with it"
}

# The installer users are told to pipe into sh. It cannot be run end to end before the
# release exists, but it names the archives, so its names must be the ones we publish.
installer_names() {
  local platform
  for platform in $PLATFORMS; do
    grep -qE "^ *${platform%%-*}/${platform#*-}\) return 0 ;;$" "$REPO/install-task.sh" || {
      echo "install-task.sh does not accept the platform $platform" >&2
      return 1
    }
  done
  echo "install-task.sh covers every published platform"
}

check "archive contents" archive_contents
check "build manifest" build_manifest
check "static binary" static_binary
check "embedded schema" embedded_schema
check "embedded completions" embedded_completions
check "repo Taskfile" own_taskfile
check "--init template" init_template
check "runner features" runner_features
check "installer platform names" installer_names

echo
echo "################ results"
for i in "${!names[@]}"; do
  printf '%-28s %s\n' "${names[$i]}" "${results[$i]}"
done
exit "$failed"

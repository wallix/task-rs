---
title: Reproducible Builds Reference
description:
  What the reproducible-build guarantee covers on each platform, and how to
  check a published artifact against its source
outline: deep
---

# Reproducible Builds Reference

Linux release binaries are reproducible from their tagged source. Archives are
deterministic when produced with the same archiver.

## What the release publishes

Each archive has a `.sha256` sidecar. Linux releases also include a
`build-info.txt` file with the binary digest and build inputs.

| Asset                         | Binary built on        | Reproducible                       |
| ----------------------------- | ---------------------- | ---------------------------------- |
| `task-linux-x86_64.tar.gz`    | pinned container image | binary yes; archive needs GNU tar  |
| `task-linux-aarch64.tar.gz`   | pinned container image | binary yes; archive needs GNU tar  |
| `task-linux-*.build-info.txt` | pinned container image | the binary digest to check against |
| `task-macos-x86_64.tar.gz`    | GitHub macOS runner    | same runner image only             |
| `task-macos-aarch64.tar.gz`   | GitHub macOS runner    | same runner image only             |
| `task-windows-x86_64.zip`     | GitHub Windows runner  | same runner image only             |
| `task-windows-aarch64.zip`    | GitHub Windows runner  | same runner image only             |

Every archive is packed by
[`package.sh`](https://github.com/wallix/task-rs/blob/main/package.sh) on a
pinned Linux runner with fixed member order, ownership, modes, and timestamps.
Archive bytes also depend on GNU tar/gzip or Info-ZIP. `build-info.txt` contains
the archiver-independent binary digest.

macOS and Windows binaries are reproducible only on the same pinned runner
image; their system toolchains are not container-pinned.

## Verify a published Linux artifact

Rebuild the tag, then compare it with the digest published in the release:

```bash
git clone https://github.com/wallix/task-rs && cd task-rs
git checkout v<X.Y.Z>          # the tag you downloaded
./build.sh                     # needs Docker (or a vk on PATH)
cd dist
base=https://github.com/wallix/task-rs/releases/download/v<X.Y.Z>
curl -fsSLO "$base/task-linux-x86_64.build-info.txt"
sha256sum -c task-linux-x86_64.build-info.txt   # -> task: OK
```

To compare the archive too, rebuild with `--package` and download its published
sidecar. This requires the same GNU tar and gzip versions as the release runner.

```bash
cd .. && ./build.sh --package && cd dist
curl -fsSLO "$base/task-linux-x86_64.sha256"   # replaces the one you just built
sha256sum -c task-linux-x86_64.sha256
```

`build.sh` is native: verify an artifact on a machine with the same architecture.
The archive sidecar is also used by `task --update`.

Releases through v4.2.0 used unpinned runners and have no `build-info.txt`.

`./build.sh --verify` performs a second build from a clean source copy and fresh
Cargo home. Release jobs run it for both Linux architectures.

## What is pinned, and why each one matters

- **Toolchain:** `rust-toolchain.toml` pins rustc.
- **Base image:** `.devcontainer/Dockerfile` pins a multi-arch digest.
- **Alpine packages:** `.devcontainer/apk-pins.txt` pins the native C toolchain.
- **Timestamps:** `SOURCE_DATE_EPOCH=0` removes build-time variation.
- **Paths:** compiler prefix maps replace checkout and Cargo registry paths.
- **Code generation:** one codegen unit avoids scheduling-dependent output.

The build runs as a non-root user with isolated Cargo home and target directories.

## What is deliberately not covered

- **macOS and Windows binaries** across different runner images.
- **Archives produced by another archiver.** Compare `build-info.txt` instead.
- **Container image layers.** The image pins inputs; its own layers may vary.
- **Host `cargo build` output.** Release guarantees apply only to `build.sh`.

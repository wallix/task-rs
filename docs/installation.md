---
title: Installation
description: Installation methods for Task
outline: deep
---

# Installation

## Binary

Download the archive for your platform from the
[releases page on GitHub](https://github.com/wallix/task-rs/releases), extract the
`task` binary, and add it to your `$PATH`. Archives are named
`task-<os>-<arch>.tar.gz` (`.zip` on Windows) — for example
`task-linux-x86_64.tar.gz` — and each ships a `task-<os>-<arch>.sha256`
checksum file.

## Updating

An installed `task` replaces itself from the same releases:

```shell
task --update              # install the latest release
task --update=4.1.0        # install a specific version
task --update --check      # only report whether a newer one exists
```

The download is checked against the published `.sha256` and has to report its own
version before it replaces the binary, and the replacement needs write access to
the directory `task` is installed in. See
[`--update`](./reference/cli.md#task-update).

## Build From Source

Task is written in Rust. Ensure you have the toolchain pinned in
[`rust-toolchain.toml`](https://github.com/wallix/task-rs/blob/main/rust-toolchain.toml)
installed (via [rustup](https://rustup.rs)).

Install the `task` binary into your Cargo bin directory:

```shell
cargo install --path crates/task
```

Or build it directly from a checkout:

```shell
cargo build --release -p task     # -> target/release/task
```

For a reproducible, statically linked (musl) binary, use the container build:

```shell
./build.sh                        # -> dist/task (+ dist/task.sha256)
```

## Verify a download

Each release archive includes a `task-<os>-<arch>.sha256` checksum. To verify a
Linux binary against its source, rebuild the tag and compare the published
binary checksum:

```shell
git checkout v<X.Y.Z> && ./build.sh
cd dist
curl -fsSLO https://github.com/wallix/task-rs/releases/download/v<X.Y.Z>/task-linux-x86_64.build-info.txt
sha256sum -c task-linux-x86_64.build-info.txt
```

See [reproducible builds](./reference/reproducible-builds.md) for platform
coverage and archive verification.

## Setup completions

You can run `task --completion <shell>` to output a completion script for any
supported shell. There are a couple of ways these completions can be added to
your shell config:

### Option 1. Load the completions in your shell's startup config (Recommended)

This method loads the completion script from the currently installed version of
task every time you create a new shell. This ensures that your completions are
always up-to-date.

::: code-group

```shell [bash]
# ~/.bashrc
eval "$(task --completion bash)"
```

```shell [zsh]
# ~/.zshrc
eval "$(task --completion zsh)"
```

```shell [fish]
# ~/.config/fish/config.fish
task --completion fish | source
```

```powershell [powershell]
# $PROFILE\Microsoft.PowerShell_profile.ps1
Invoke-Expression  (&task --completion powershell | Out-String)
```

:::

### Option 2. Copy the script to your shell's completions directory

This method requires you to manually update the completions whenever Task is
updated. However, it is useful if you want to modify the completions yourself.

::: code-group

```shell [bash]
task --completion bash > /etc/bash_completion.d/task
```

```shell [zsh]
task --completion zsh  > /usr/local/share/zsh/site-functions/_task
```

```shell [fish]
task --completion fish > ~/.config/fish/completions/task.fish
```

:::

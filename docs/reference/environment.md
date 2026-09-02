---
title: Environment Reference
description: A reference for the Taskfile environment variables
outline: deep
---

# Environment Reference

Task-specific environment variables use the `TASK_` prefix. Equivalent
[command-line flags](./cli.md) take priority.

## Variables

Priority: CLI flags > environment variables > defaults.

### `TASK_VERBOSE`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Enable verbose output for all tasks

### `TASK_SILENT`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Disables echoing of commands

### `TASK_COLOR`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `true`
- **Description**: Enable colored output

### `TASK_DISABLE_FUZZY`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Disable fuzzy matching for task names

### `TASK_CONCURRENCY`

- **Type**: `integer`
- **Description**: Limit number of tasks to run concurrently

### `TASK_FAILFAST`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: When running tasks in parallel, stop all tasks if one fails

### `TASK_DRY`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Compiles and prints tasks in the order that they would be run, without executing them

### `TASK_ASSUME_YES`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Assume "yes" as answer to all prompts

### `TASK_INTERACTIVE`

- **Type**: `boolean` (`true`, `false`, `1`, `0`)
- **Default**: `false`
- **Description**: Prompt for missing required variables

### `TASK_TEMP_DIR`

Defines the location of Task's temporary directory which is used for storing
checksums and temporary metadata. Can be relative like `tmp/task` or absolute
like `/tmp/.task` or `~/.task`. Relative paths are relative to the root
Taskfile, not the working directory. Defaults to: `./.task`.

### `TASK_CORE_UTILS`

This env controls whether the Bash interpreter will use its own
core utilities implemented in Go, or the ones available in the system.
Valid values are `true` (`1`) or `false` (`0`). By default, this is `true` on
Windows and `false` on other operating systems. We might consider making this
enabled by default on all platforms in the future.

### `TASK_NO_REAP`

When a run *fails* after being torn down part-way — a `failfast:` dependency
failing, a third interrupt or a `SIGTERM` forcing shutdown — the commands it
abandoned keep running, so Task walks its own process tree and stops what it
finds. It cannot
attribute a process to the task that started it, so a job a task left running
deliberately is stopped along with the rest. Set `TASK_NO_REAP=1` to turn the
sweep off and leave everything running, including at a forced shutdown. Valid
values are `true` (`1`) or `false` (`0`); anything else leaves it on.

A run that succeeds never sweeps, even if a task was abandoned along the way.

`TASK_NO_REAP=1` also stops the signal relay: from the second interrupt on,
Task passes the signal to the commands it started, and the switch suppresses
that too, so a command only ever sees what the terminal delivered to it
directly.

### `TASK_CACHE_OCI_USER`, `TASK_CACHE_OCI_PASSWORD`

HTTP Basic credentials for a `cache.url: oci://...` registry, used when the URL
carries no `user:pass@` credentials of its own and `TASK_VK_API_KEY` is
unset. A `cache.lock: vk://...` lock also uses the pair when its URL has no
credentials and neither bearer-token variable is set.

### `TASK_VK_API_KEY`

The vk-registry API key (a bearer token) for a `cache.vk:` model whose block
sets no `api_key` of its own. Also read by a `cache.url: oci://...` registry
when the URL carries no `user:pass@` credentials, taking precedence over
`TASK_CACHE_OCI_USER` / `TASK_CACHE_OCI_PASSWORD`.

A `cache.lock: vk://...` lock reads the same token when `TASK_VK_LOCK_TOKEN` is
unset. If both tokens are unset, it uses the OCI Basic pair instead. The lock
and cache APIs can share a registry.

### `TASK_VK_LOCK_TOKEN`

Bearer token for a `cache.lock: vk://...` distributed lock. A block's `api_key`
and URL Basic credentials override it. When unset, the lock falls back to
`TASK_VK_API_KEY`, then the OCI Basic pair. It is sent on every acquire,
renew and release, in the clear unless the URL is `vks://`.

### `TASK_CACHE_OCI_CA`

Path to a PEM file holding an extra trust anchor for a `cache.url: oci://...`
registry whose certificate the system store does not chain to (a self-signed
corp registry), used when the URL carries no `?ca=` of its own. A relative path
is relative to the directory `task` is invoked from.

A `cache.lock: vks://...` reads the same variable, since the lock API is served
by the same registry.

The certificate the registry presents must be a leaf (`CA:FALSE`); a CA
certificate served as the TLS end-entity is rejected (`CaUsedAsEndEntity`)
whether or not it is also the configured anchor.

The registry's other settings — credentials, the local chunk store, plain HTTP
— are documented with `cache` in the [schema reference](schema.md#cache).

### `FORCE_COLOR`

Force color output usage.

### Custom Colors

All color variables are [ANSI color codes][ansi]. You can specify multiple codes
separated by a semicolon. For example: `31;1` will make the text bold and red.
Task also supports 8-bit color (256 colors). You can specify these colors by
using the sequence `38;2;R:G:B` for foreground colors and `48;2;R:G:B` for
background colors where `R`, `G` and `B` should be replaced with values between
0 and 255.

For convenience, we allow foreground colors to be specified using shorthand,
comma-separated syntax: `R,G,B`. For example, `255,0,0` is equivalent to
`38;2;255:0:0`.

A table of variables and their defaults can be found below:

| ENV                         | Default |
| --------------------------- | ------- |
| `TASK_COLOR_RESET`          | `0`     |
| `TASK_COLOR_RED`            | `31`    |
| `TASK_COLOR_GREEN`          | `32`    |
| `TASK_COLOR_YELLOW`         | `33`    |
| `TASK_COLOR_BLUE`           | `34`    |
| `TASK_COLOR_MAGENTA`        | `35`    |
| `TASK_COLOR_CYAN`           | `36`    |
| `TASK_COLOR_BRIGHT_RED`     | `91`    |
| `TASK_COLOR_BRIGHT_GREEN`   | `92`    |
| `TASK_COLOR_BRIGHT_YELLOW`  | `93`    |
| `TASK_COLOR_BRIGHT_BLUE`    | `94`    |
| `TASK_COLOR_BRIGHT_MAGENTA` | `95`    |
| `TASK_COLOR_BRIGHT_CYAN`    | `96`    |

[ansi]: https://en.wikipedia.org/wiki/ANSI_escape_code

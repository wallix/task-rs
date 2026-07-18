---
title: Templating Reference
description:
  Guide to Task's templating system — native Jinja (the default), the legacy Go
  text/template dialect, special variables, and available functions.
outline: deep
---

# Templating Reference

Task renders the string values in a Taskfile with a templating engine so they
can be computed dynamically. Templates are written between double curly braces
<span v-pre>`{{` and `}}`</span> (expressions) and, in Jinja, `{% … %}`
(statements).

Task supports **two template dialects**:

- **Jinja** (the default) — native [minijinja](https://github.com/mitsuhiko/minijinja),
  a Jinja2-compatible engine. This is the recommended dialect for new Taskfiles.
- **Go `text/template`** (legacy, **deprecated**) — the original Task dialect, a
  limited subset of which is still supported for backwards compatibility.

## Choosing a dialect

The dialect is resolved **per file**:

- If a file sets the top-level `templater:` field, that wins:

  ```yaml
  version: '3'
  templater: jinja   # or: go
  ```

- Otherwise the dialect is **auto-detected** from the file's syntax. Leading-dot
  access (`{{.VAR}}`), Go control words (`{{if}}`, `{{range}}`), and Go comments
  (`{{/* … */}}`) mark a file as Go; anything else (including a file with no
  templates) is treated as Jinja.

Files that resolve to the Go dialect emit a one-time deprecation warning. Go
template support will be removed in a future release. Convert a Taskfile with:

```bash
task --migrate          # preview the Jinja conversion on stdout
task --migrate --write  # rewrite the file in place and add `templater: jinja`
```

To silence the deprecation warning in the meantime, set
`TASK_NO_GO_DEPRECATION=1`.

## Jinja templating (default)

### Variable interpolation

Variables are referenced by name — no leading dot:

```yaml
version: '3'
templater: jinja

tasks:
  hello:
    vars:
      MESSAGE: 'Hello, World!'
    cmds:
      - 'echo {{ MESSAGE }}'
```

A variable that is not defined renders as an empty string.

### Filters

Values are transformed with the pipe (`|`) filter syntax:

```yaml
cmds:
  - 'echo {{ NAME | upper }}'                 # JOHN DOE
  - 'echo {{ MESSAGE | trim }}'               # trims surrounding whitespace
  - 'echo {{ MISSING | default("fallback") }}'
```

Filters can be chained: `{{ CSV | splitList(",") | join(" ") }}`.

### Function calls

Functions use parentheses:

```yaml
cmds:
  - 'echo {{ OS() }}/{{ ARCH() }}'
  - 'echo {{ joinPath(ROOT_DIR, "bin", "app") }}'
  - 'echo {{ env("HOME") }}'
```

### Conditionals

```yaml
cmds:
  - 'echo {% if CI %}github-actions{% else %}local{% endif %}'
```

Comparisons and boolean logic use native operators (`==`, `!=`, `<`, `>`,
`and`, `or`, `not`, `in`):

```yaml
cmds:
  - '{% if OS() == "linux" and not DRY_RUN %}./deploy.sh{% endif %}'
```

### Loops

```yaml
cmds:
  - |
    {% for name in ["alice", "bob", "charlie"] %}
    echo "Hello {{ name }}"
    {% endfor %}
```

### More

Jinja mode is native minijinja, so its full syntax is available — `{% set %}`,
tests (`is defined`, `is none`, …), the standard filter set (see
[Functions and filters](#functions-and-filters) below), and arithmetic
(`{{ 1 + 2 }}`). See the
[minijinja documentation](https://docs.rs/minijinja/latest/minijinja/syntax/index.html)
for the complete syntax.

## Go templating (legacy, deprecated)

::: warning

The Go `text/template` dialect is **deprecated** and only a subset is supported.
Prefer Jinja for new Taskfiles and migrate existing ones with `task --migrate`.

:::

Go templates reference variables with a leading dot and use Go's pipeline and
control-flow syntax:

```yaml
version: '3'

tasks:
  hello:
    vars:
      MESSAGE: 'Hello, World!'
      HAPPY: true
    cmds:
      - 'echo {{.MESSAGE}}'
      - 'echo {{if .HAPPY}}:){{else}}:({{end}}'
      - 'echo {{.NAME | trim | upper}}'
```

Supported Go constructs:

- Interpolation and nested field access: `{{.VAR}}`, `{{.MAP.KEY}}`.
- Conditionals: `{{if …}}`, `{{else if …}}`, `{{else}}`, `{{end}}`.
- Pipelines and the mapped functions listed under
  [Functions and filters](#functions-and-filters).
- The builtins `and`, `or`, `not`, `eq`, `ne`, `lt`, `le`, `gt`, `ge`, `index`,
  `len`, and Go comments `{{/* … */}}`.
- Parenthesised sub-expressions: `{{ regexReplaceAll "[^a-z]" (trunc 48 .TASK) "-" }}`.

**Not supported** (these raise an error — migrate to Jinja instead): `range` and
`with` loops, and the wider [slim-sprig] function library that upstream Task
offered (list/dict/date/math/encoding helpers, `uuid`, `spew`, and so on). Any
`{% … %}` or `{# … #}` in a Go-dialect file is treated as literal text, exactly
as Go `text/template` would.

## Special variables

Task provides these variables in every template. They are the same in both
dialects — only the access syntax differs (`{{ TASK }}` in Jinja, `{{.TASK}}` in
Go). Examples below use Jinja.

### CLI

| Variable | Type | Description |
| --- | --- | --- |
| `CLI_ARGS` | `string` | Extra arguments after `--`, as a single string |
| `CLI_ARGS_LIST` | `list` | Extra arguments after `--`, shell-split into a list |
| `CLI_FORCE` | `bool` | Whether `--force` or `--force-all` was set |
| `CLI_SILENT` | `bool` | Whether `--silent` was set |
| `CLI_VERBOSE` | `bool` | Whether `--verbose` was set |
| `CLI_ASSUME_YES` | `bool` | Whether `--yes` was set |

```yaml
tasks:
  test:
    cmds:
      - cargo test {{ CLI_ARGS }}   # task test -- --nocapture
```

### Task

| Variable | Type | Description |
| --- | --- | --- |
| `TASK` | `string` | Name of the current task |
| `ALIAS` | `string` | Alias used to call the task, otherwise the task name |
| `TASK_EXE` | `string` | The `task` executable name or path |

### File paths

| Variable | Type | Description |
| --- | --- | --- |
| `ROOT_TASKFILE` | `string` | Absolute path of the root Taskfile |
| `ROOT_DIR` | `string` | Absolute path of the root Taskfile's directory |
| `TASKFILE` | `string` | Absolute path of the current (included) Taskfile |
| `TASKFILE_DIR` | `string` | Absolute path of the current Taskfile's directory |
| `TASK_DIR` | `string` | Absolute path the task runs in |
| `USER_WORKING_DIR` | `string` | Absolute path `task` was invoked from |

### Status and cache

| Variable | Type | Description |
| --- | --- | --- |
| `CHECKSUM` | `string` | Checksum of the task's `sources` (available in `status`, and in the `cache` `url`/`lock` templates) |

```yaml
tasks:
  build:
    sources: ['**/*.rs']
    cache:
      url: 'oci://registry.example.com/cache:{{ urlsafe(TASK) }}-{{ CHECKSUM }}'
    cmds:
      - cargo build --release
```

### Loop

| Variable | Type | Description |
| --- | --- | --- |
| `ITEM` | `any` | The current value when iterating with a command's `for` property (rename with `as`) |

```yaml
tasks:
  greet:
    cmds:
      - for: [alice, bob]
        cmd: echo "Hello {{ ITEM }}"
```

### Defer

| Variable | Type | Description |
| --- | --- | --- |
| `EXIT_CODE` | `int` | The failed command's exit code — only in a `defer`, and only when non-zero |

### System

| Variable | Type | Description |
| --- | --- | --- |
| `TASK_VERSION` | `string` | The running version of Task |

## Functions and filters

The functions below are provided by Task in **both** dialects. In Jinja they are
called as functions (`joinPath(a, b)`) or filters (`value | trimPrefix("x")`); in
Go they are called in pipeline/space-separated form (`joinPath a b`,
`.VALUE | trimPrefix "x"`).

### Platform and environment

| Function | Description |
| --- | --- |
| `OS()` | The operating system (`linux`, `darwin`, `windows`, …) |
| `ARCH()` | The CPU architecture (`amd64`, `arm64`, …) |
| `numCPU()` | The number of CPUs available |
| `exeExt()` | The executable extension for the OS (`.exe` on Windows, else empty) |
| `env(name)` | The value of an environment variable, or empty if unset |

### Paths

| Function | Description |
| --- | --- |
| `joinPath(a, b, …)` | Join and clean path segments |
| `base(path)` | The final path element |
| `dir(path)` | The parent directory |
| `ext(path)` | The file extension (including the dot) |
| `isAbs(path)` | Whether the path is absolute |
| `fromSlash(path)` | Convert `/` to the OS path separator |
| `toSlash(path)` | Convert the OS path separator to `/` |

### Strings

| Function / filter | Description |
| --- | --- |
| `trim`, `trimAll(cutset)`, `trimPrefix(prefix)`, `trimSuffix(suffix)` | Trim whitespace or a given cutset/affix |
| `lower`, `upper`, `title` | Change case |
| `contains(substr)`, `hasPrefix(prefix)`, `hasSuffix(suffix)` | Substring tests |
| `replace(old, new)` | Replace all occurrences |
| `trunc(n, s)` | First `n` characters (or last `-n` if negative) |
| `regexReplaceAll(pattern, s, repl)` | Replace all regex matches |
| `quote(s)`, `squote(s)` | Wrap in double / single quotes |
| `urlsafe(s)` | Percent-encode for use in URLs and cache keys |
| `catLines(s)` | Replace newlines with spaces |
| `splitLines(s)` | Split into a list of lines |

### Lists

| Function / filter | Description |
| --- | --- |
| `s \| splitList(sep)` | Split a string into a list on `sep` |
| `list \| join(sep)` | Join a list into a string with `sep` |
| `list \| first`, `list \| last` | The first / last element |
| `len(x)` | Length of a list, map, or string |
| `splitArgs(s)` | Shell-split a string into an argument list |
| `index(coll, k…)` | Successive index/key lookups (`index(MATCH, 0)`) |

### Comparison and logic (Go dialect)

`and`, `or`, `not`, `eq`, `ne`, `lt`, `le`, `gt`, `ge` and `default(value)` are
available for the Go dialect. In Jinja, use the native operators (`==`, `!=`,
`<`, `and`, `or`, `not`, `in`) and the `default` filter instead.

### Standard Jinja filters

In the Jinja dialect, minijinja's built-in filters and functions are also
available — for example `default`, `length`, `join`, `first`, `last`, `reverse`,
`sort`, `unique`, `map`, `select`, `int`, `float`, `tojson`, and `urlencode`.
See the
[minijinja filter reference](https://docs.rs/minijinja/latest/minijinja/filters/index.html)
for the full list.

[slim-sprig]: https://github.com/go-task/slim-sprig

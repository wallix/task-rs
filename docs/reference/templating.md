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

  Detection is textual: a dot that starts an identifier and is not preceded by
  a letter, digit, `_`, `)`, `"` or `'` marks the file as Go — **even inside a
  string**. A native Jinja file containing, say,
  `{{ PATH | replace("/.git", "") }}` is therefore misread and fails to
  translate. Set `templater: jinja` explicitly on such a file.

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

An action also ends at the first `}}`, even one inside a string literal, so
`{{ .P | replace "}}" "" }}` cannot be written directly — build the braces
from a variable, or use the Jinja dialect.

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

`default`, `title`, `join`, `first` and `last` are the exception: in Jinja the
*filter* of each of those names is Jinja's own, not Task's — see
[sprig semantics after a pipe](#sprig-semantics-after-a-pipe-go-dialect).

Most helpers that take a subject accept it in either position, but at opposite
ends: the function form takes it **last** (`trimSuffix ".po" .ITEM`), which is
what lets the pipeline form take it first (`.ITEM | trimSuffix ".po"`). `trunc`
and `regexReplaceAll` are function-only.

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
| `lower`, `upper`, `title`† | Change case |
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
| `splitList(sep, s)`, `s \| splitList(sep)` | Split a string into a list on `sep` |
| `join(sep, list)`†, `list \| join(sep)` | Join a list into a string with `sep` |
| `first(list)`†, `last(list)`†, `list \| first`, `list \| last` | The first / last element |

† In the Jinja dialect the filter spelling is minijinja's own; only the call
form carries sprig's meaning.
| `len(x)` | Length of a list, map, or string |
| `splitArgs(s)` | Shell-split a string into an argument list |
| `index(coll, k…)` | Successive index/key lookups (`index(MATCH, 0)`) |

### Comparison and logic (Go dialect)

`and`, `or`, `not`, `eq`, `ne`, `lt`, `le`, `gt`, `ge` are available for the Go
dialect. In Jinja, use the native operators (`==`, `!=`, `<`, `and`, `or`,
`not`, `in`) instead. `default(fallback, value)` is a function in both dialects;
in Jinja the `| default(fallback)` filter is Jinja's own — see below.

### sprig semantics after a pipe (Go dialect)

`default`, `title`, `join`, `first` and `last` mean something different in
[slim-sprig] than the minijinja filters of the same name. In a Go-syntax
Taskfile the sprig meaning wins, in both call and pipe position.

Jinja Taskfiles are **not** affected — `{{ COUNT | default(10) }}` there is
Jinja's own filter and still yields `0` for a `COUNT` of `0`. The Go dialect
gets sprig's meaning by translating `{{ .X | default "y" }}` into the call
`default("y", X)` rather than into a filter, so `task --migrate` writes that
call into the converted file and the migrated Taskfile keeps rendering what it
rendered as Go. Keep the call form when editing a migrated file: rewriting it
to `X | default("y")` silently switches to Jinja's meaning.

Where the two differ:

- `default` substitutes its fallback for any empty value (`""`, `0`, `false`, an
  empty list), not only an undefined one.
- `title` uppercases the first letter of every word and leaves the rest of the
  word alone, so `HELLO world` becomes `HELLO World` where Jinja's `title`
  gives `Hello World`.
- `join` treats a non-list as a one-element list, so a string joins to itself
  instead of to its characters.
- `first` and `last` render empty for a value they cannot iterate — a number, a
  boolean, an undefined variable — where Jinja's raise an error. A string still
  yields its first or last character.

Every other helper keeps one meaning in both dialects, and migrates to the
idiomatic filter form (`{{ .X | trimSuffix ".po" }}` becomes
`{{ X | trimSuffix(".po") }}`).

### Standard Jinja filters

In the Jinja dialect, minijinja's built-in filters and functions are also
available — for example `default`, `title`, `join`, `first`, `last`, `length`,
`reverse`, `sort`, `unique`, `map`, `select`, `int`, `float`, `tojson`, and
`urlencode`, all with their standard Jinja meaning. See the
[minijinja filter reference](https://docs.rs/minijinja/latest/minijinja/filters/index.html)
for the full list.

[slim-sprig]: https://github.com/go-task/slim-sprig

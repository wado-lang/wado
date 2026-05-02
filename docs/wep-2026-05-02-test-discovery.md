# WEP: Test Discovery

## Context

`wado test` walks a project for test files, compiles them under the `test`
world, and runs the test blocks they declare. The walker did not have a
formal definition: there were no rules for `.gitignore`, submodules, hidden
files, package boundaries, or per-package configuration. Real projects (this
repo included) accumulate fixture trees and nested packages that need
explicit expression, and discovery has to behave the same way across
sub-packages so that conventions don't silently diverge.

This WEP defines how `wado test` discovers files, what it does with them,
and how the result is reported.

## Decision

### Discovery scope

Walk the project root for `*.wado` files. Skip:

- Entries matched by `.gitignore` (and ancestor `.gitignore`s, with the same
  semantics git applies — last matching rule wins, directory-only patterns
  end in `/`, negation with `!`, `**` for multi-segment wildcards). The
  parser is in-process; no `git` binary is required.
- Directories listed in the project's `.gitmodules`.
- Dot-prefixed files and directories (`.git`, `.vscode`, ...).
- Subtrees rooted at a nested `wado.toml`. Each such directory is a separate
  package and is visited as its own `wado test` invocation (cargo workspace
  style).
- Entries matched by `[test].exclude` in the package's `wado.toml`, or by
  `--exclude <pattern>` on the CLI.

Symbolic links are followed; cycles are detected by tracking visited
canonical paths.

`#![generated]` files are **not** skipped. Parsing them is required to detect
the attribute, and once parsed there is no reason to stop short of compiling
them — generator regressions become test failures.

### `wado.toml` configuration

```toml
[test]
exclude = [
    "wado-compiler/tests/**",
    "wado-from-idl/tests/**",
]
```

`exclude` is a list of shell-style glob patterns relative to the package
root. The CLI's `--exclude <pattern>` flag (repeatable) extends the manifest
list at invocation time.

`*` honours path separators, so `src/*.wado` matches direct children of
`src/` only. Use `**` to cross directories. As a convenience, `dir/**` also
excludes the bare `dir` entry (otherwise the walker would still descend into
it, since `**` requires at least one trailing path component).

### Filter

`--filter <pattern>` (repeatable) matches discovered file paths using
shell-style wildcards (`*`, `?`, `[...]`). Not regex. A path is kept when
any pattern matches. The pattern is matched against the path string the
walker emits (relative to the invocation root, with no leading `./`).

### World

All discovered files compile under the `test` world. The world is an
entry-point selector — `export fn run` and `export async fn handle` from
other worlds still type-check under `test`, so any `.wado` file is a valid
discovery target regardless of the world it's normally built for.

### Test block execution scope

Only test blocks declared in the **entry module** (the file passed to the
compiler) are executed. Test blocks reachable through `use` imports are
compiled but not registered.

This keeps file-by-file discovery from multiplying test executions by the
number of importers each test-bearing module has, and yields a stable,
source-order test schedule.

### Files without test blocks

Parsed and compiled for validation. No Wasm is emitted, nothing is executed.
A compile failure in such a file fails the `wado test` invocation (non-zero
exit) and is reported on the compile axis of the summary, separate from
test pass/fail.

### Reporting

Three axes in the summary:

- **compile**: passed / failed
- **test**: passed / failed
- **todo**: pending / resolved (when `#[TODO]` is in play)

When more than one package ran (the no-args case under sub-package
recursion), each package emits its own three-axis line under a
`=== package: <label> ===` banner, followed by an `=== aggregate ===`
block summing the axes across packages.

Any non-zero compile-failed, test-failed, or `todo resolved` count makes the
process exit non-zero.

### Sub-package recursion

When `wado test` is invoked without explicit path arguments, the walker
treats every nested `wado.toml` as a separate package context: the recursion
applies that package's own `[test].exclude`, the per-package summary line is
labelled with the package's relative path, and the parent walk does not
descend into the sub-package's source. Explicit path arguments collapse to a
single root run; sub-package recursion only fires for the project-wide case.

### CLI flags

| Flag                    | Effect                                                                  |
| ----------------------- | ----------------------------------------------------------------------- |
| `--filter <pat>`        | Keep paths matching the wildcard. Repeatable; OR-combined.              |
| `--exclude <pat>`       | Drop paths matching the wildcard. Repeatable; extends `[test].exclude`. |
| `-O<n>`                 | Optimization level for the compile step.                                |
| `-p <N>` / `--parallel` | Parallel test workers.                                                  |
| `--dir <host[::guest]>` | Preopen a host directory for WASI filesystem access.                    |
| `--no-dir`              | Disable the default `(".", ".")` preopen.                               |

## Consequences

### User-visible

- `wado test` (no arguments) walks the project root from cwd, honouring
  `.gitignore`, `.gitmodules`, dot-prefixed paths, nested `wado.toml`
  boundaries, and `[test].exclude` automatically. No flags required for the
  common case.
- `*_test.wado` is no longer required for discovery. It remains a
  recommended convention for files that are exclusively tests, as a
  human-readable signal.
- `--filter` is path-based wildcard matching; users expecting regex must
  adjust.
- A compile error in any reached `.wado` file fails `wado test`, even when
  the file has no test blocks. This is the compile-only validation axis.
- Test blocks in transitively imported modules are not executed
  per-importer; they only run in the file that owns them.

### Implementation

- `wado-cli/src/discover.rs`: walker with in-process `.gitignore` /
  `.gitmodules` parsing, dot-prefix filter, nested-`wado.toml` boundaries,
  manifest + CLI excludes, and symlink cycle detection. Glob patterns are
  matched with `require_literal_separator: true`; `dir/**` is augmented
  with the bare `dir` prefix so directory excludes behave intuitively.
- `wado-cli/src/test.rs`: turns the walker output into a `Vec<PackageRun>`
  (root + recursively discovered sub-packages), applies `--filter`, drives
  the compile/run pipeline per package, and emits the per-package and
  aggregate three-axis summaries.
- `wado-manifest/`: `[test].exclude: Vec<String>` on the manifest schema.
- `wado-compiler/src/loader.rs`: `#include_str` / `#include_bytes` path
  resolution recognises the entry module so that running with a
  `./pkg/src/main.wado`-style argument no longer doubles the base path.
- `wado-compiler` (test runner side): only entry-module test blocks are
  registered as exports — sealed by a fixture pair under
  `tests/fixtures/test_imported_test_block_skipped.wado` that traps in an
  imported test block and asserts that `wado test` never invokes it.

### Trade-offs

- Discovery cost grows with the project size. Mitigated by parallel
  compilation and by `[test].exclude` for known-large fixture trees.
- Sub-package recursion duplicates some bookkeeping (each package gets its
  own summary line) but keeps each package's results attributable.

## Open question: package-local assets

`wado test` runs every package in the same WASI sandbox: there is one
`--dir` setup for the whole invocation. That is sufficient when test bodies
use `#include_str` / `#include_bytes` to embed their fixtures at compile
time (the dominant case, and what `benchmark/*` does), but it leaves no
clean answer for a sub-package whose tests genuinely need to read files at
runtime — the per-package `wado.toml` cannot ask the runner to preopen its
own directory.

The likely shape of a follow-up: a manifest declaration such as
`[package].assets = "fixtures"`, combined with a compile-time parameter
([WEP: Compile-Time Parameters](./wep-2026-04-26-compile-time-params.md))
that exposes the package's preopen name to source. Tests then read
`Preopens::read_file(`{ASSETS_DIR}/queries.sql`)` without hard-coding host
paths, and `wado test` arranges the per-package preopens during sub-package
recursion.

This is intentionally **not** part of this WEP. The 95-percent answer is
`#include_str`; the runtime path earns its complexity only when several
real consumers want it. The open question is recorded here so that if those
consumers appear, the design starts from a known position rather than from
scratch.

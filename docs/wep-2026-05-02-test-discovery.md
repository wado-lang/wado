# WEP: Test Discovery Refinement

## Context

[WEP: Synopsis Tests](./wep-2026-04-26-synopsis-tests.md) changed `wado test`
discovery from `**/*_test.wado` to `**/*.wado` so that synopsis blocks living
in implementation files are reached by the runner. That WEP left the concrete
discovery rules ("the same project-root and ignore rules currently in effect")
unspecified.

This WEP fills in those rules and adds the surrounding mechanics needed for
the discovery change to be usable in real projects — most immediately, this
repository, which contains a large `wado-compiler/tests` fixture tree that
must not be picked up as project-level tests.

## Decision

### Discovery scope

Walk the project root for `*.wado` files. Skip:

- Entries matched by `.gitignore` (and ancestor `.gitignore`s, as git would).
- Git submodule directories.
- Dot-prefixed files and directories (`.git`, `.vscode`, ...).
- Subtrees rooted at a nested `wado.toml` — those are separate packages and
  are visited by recursing into them as their own `wado test` invocation
  (cargo workspace style).
- Entries listed in `[test].exclude` of the project's `wado.toml`.

Symbolic links are followed; cycles are detected by tracking visited canonical
paths.

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

`exclude` is a list of glob patterns relative to the package root. The CLI
gains `--exclude <pattern>` to extend the manifest list at invocation time.

### Filter

`--filter <pattern>` matches discovered file paths using shell-style
wildcards (`*`, `?`, `[...]`). Not regex. The pattern is matched against the
path relative to the package root.

### World

All discovered files compile under the `test` world. Worlds are entry-point
selectors and coexist within a single component, so files written for
`wasi:cli/command` or `wasi:http/service` still type-check under `test`.

### Test block execution scope

Only test blocks declared in the **entry module** (the file passed to the
compiler) are executed. Test blocks reachable through `use` imports are
compiled but not registered.

This is necessary so that running `wado test` on a project does not multiply
test executions by the number of importers each test-bearing module has, and
so that file-by-file discovery yields a stable, source-order test schedule.
The current behaviour must be audited and corrected if it deviates.

### Files without test blocks

Parsed and compiled for validation. No Wasm is emitted, nothing is executed.
A compile failure in such a file fails the `wado test` invocation (non-zero
exit) and is reported on a separate axis from test pass/fail.

### Reporting

Three axes in the summary:

- compile: passed / failed
- test: passed / failed
- skipped (filtered out, `#[TODO]`, etc.)

Any non-zero compile-failed or test-failed count makes the process exit
non-zero.

### This repository

Add a `wado.toml` at the repo root so that:

- Sub-package recursion (`wado-cli/`, `wado-compiler/`, ...) is exercised by
  the project's own CI.
- Fixture trees under `wado-compiler/tests/` and similar are excluded via
  `[test].exclude`.

## Consequences

### User-visible

- `wado test` honours `.gitignore`, submodules, dot-prefixed paths, nested
  `wado.toml` boundaries, and `[test].exclude` automatically. No flags
  required for the common case.
- `--filter` is path-based wildcard matching; users coming from regex tools
  must adjust.
- A compile error in a previously unreached `.wado` file now fails
  `wado test`. This is intentional — see Synopsis Tests WEP.
- Test blocks in transitively imported modules are no longer executed
  per-importer. If an existing project relied on this, those tests must move
  to the file that owns them.

### Implementation

- `wado-cli/src/test.rs`: replace the discovery walker with one that consults
  `ignore` (gitignore), submodule list, dot-prefix filter, nested `wado.toml`
  boundaries, and the `[test].exclude` glob set. Follow symlinks with cycle
  detection.
- `wado-cli/src/test.rs`: recurse into nested packages by re-invoking the
  discovery + run pipeline rooted at each nested `wado.toml`.
- `wado-manifest/`: add `[test].exclude: Vec<String>` to the manifest schema.
- `wado-cli`: add `--exclude <pattern>` (repeatable) and change `--filter` to
  wildcard semantics.
- `wado-compiler` (test runner side): ensure only entry-module test blocks
  are registered. Audit the current path; fix if imported test blocks leak
  into the schedule.
- `wado-cli/src/test.rs`: emit the three-axis summary and propagate a
  non-zero exit on compile or test failures.
- Add a top-level `wado.toml` to this repository configured to exercise the
  exclude list and sub-package recursion.

### Trade-offs

- Discovery cost grows with the project size. Mitigated by parallel
  compilation and by `[test].exclude` for known-large fixture trees.
- Sub-package recursion duplicates some bookkeeping (each package gets its
  own summary line) but keeps each package's results attributable.

## TODOs

- [ ] Implement discovery walker with gitignore / submodule / dot-prefix /
      nested-`wado.toml` / `[test].exclude` rules and symlink cycle detection.
- [ ] Add `[test].exclude: Vec<String>` to `wado-manifest`.
- [ ] Add `--exclude` and switch `--filter` to wildcard path matching.
- [ ] Audit and, if necessary, restrict test-block execution to the entry
      module.
- [ ] Implement compile-only validation for files without test blocks.
- [ ] Emit three-axis summary and propagate exit code.
- [ ] Recurse into nested `wado.toml` packages.
- [ ] Add a root `wado.toml` to this repository wiring up the exclude list.
- [ ] Update `docs/cheatsheet.md` and the CLI subcommands doc to describe the
      new discovery rules.

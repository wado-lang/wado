# WEP: Synopsis Tests

## Context

Wado has two related but disconnected features today:

- `test { ... }` blocks (`wado-compiler/src/parser.rs:444-500`) define module-level
  tests. The runner is `wado test` (`wado-cli/src/test.rs`), which discovers
  files matching `**/*_test.wado` and executes their tests under the `test`
  world.
- `wado doc` (WEP 2026-02-28) renders public API documentation from `///` and
  `//!` comments. It extracts text only — there is no notion of an executable
  example.

Test blocks are already permitted in implementation files (not only
`*_test.wado`); the parser does not impose a filename restriction. What is
restricted today is _discovery_: `wado test` only walks `*_test.wado`, so any
test placed in an implementation file is silently ignored unless run by an
explicit path.

### Problem

Documentation without runnable examples rots. The Java/Javadoc tradition of
hand-written, never-executed snippets is the canonical bad outcome — examples
diverge from the API and readers learn to distrust them. Conversely, the
perldoc SYNOPSIS section worked because it was short, hand-written, and placed
at the head of the module — it served as the reader's entry point.

We want both properties at once for Wado:

- The hand-written, narrative quality of perldoc SYNOPSIS.
- The freshness guarantee of an executable test.

### Considered alternatives (rejected)

1. **Code blocks inside `///` doc comments (Rust `rustdoc` doctests).**
   Most powerful, but parsing Wado source inside a comment multiplies the
   number of front-ends to maintain: lexer, parser, formatter, syntax
   highlighter, and LSP would each need a "Wado-inside-comment" mode. The
   hidden-setup convention (Rust's leading-`#` lines) is a known anti-pattern:
   examples whose visible body lies are worse than no examples. Rust adopted
   this when its parser was already mature; Wado's parser is still evolving and
   the cost is prohibitive.

2. **Dedicated `synopsis { ... }` syntax (D `unittest`, Nim
   `runnableExamples`).**
   Cleaner intent, but a synopsis is structurally a test: a runnable block with
   assertions. The only thing that differs is the documentation-rendering side
   effect. New syntax adds parser/formatter/LSP work for no semantic gain over
   reusing the test machinery.

## Decision

### The `#[synopsis]` attribute

A `test` block may carry `#[synopsis]`. Such a test:

- Is executed by `wado test` like any other test (so it cannot go stale).
- Is additionally rendered by `wado doc` as part of the enclosing module's
  Synopsis section.

```wado
//! Geometric primitives.

#[synopsis]
test {
    let p = Point { x: 0, y: 0 };
    let q = Point { x: 3, y: 4 };
    assert p.distance(&q) == 5.0;
}

pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn distance(&self, other: &Point) -> f64 { ... }
}
```

Rules:

- `#[synopsis]` is module-scoped. It documents the enclosing file, not a
  specific item. This matches the perldoc SYNOPSIS convention: a single
  reader-facing entry point at the top of the module.
- Multiple `#[synopsis]` tests per module are allowed and rendered in source
  order.
- A `#[synopsis]` test is conventionally unnamed (`test { ... }`). The
  description-string form `test "..." { ... }` is reserved for ordinary tests
  where the string distinguishes one assertion case from another; a synopsis is
  the example, not a case among many, so the string adds noise. The unnamed
  form already exists in the grammar and finds its primary use here.
- `#[synopsis]` may be combined with `#[expect_trap]`, `#[TODO]`, and
  `#[timeout_ms]`. `#[expect_trap]` is the natural way to show an intended
  panic; `#[TODO]` lets a draft synopsis live in the source without breaking
  CI.

Per-item binding (`#[synopsis(for = some::path)]`) is intentionally **not**
introduced in this WEP. It can be added later if demand appears. Keeping
synopses module-scoped avoids the "string path silently rots on rename" failure
mode and matches the level at which perldoc readers actually decide whether to
use a module.

### Universal test discovery

`wado test` discovery is changed from `**/*_test.wado` to `**/*.wado` (with
the same project-root and ignore rules currently in effect).

For each discovered file:

- If the file contains one or more `test` blocks, those tests are registered
  and executed.
- If the file contains no `test` blocks, it is parsed and compiled (validated)
  but registers nothing.

The historical `*_test.wado` naming is no longer required by the runner. It
remains a recommended convention for files that contain _only_ tests, as a
human-readable signal.

This change is necessary to make `#[synopsis]` useful: a synopsis must live
next to the code it documents (the implementation file), and that file must be
visited by `wado test` for the synopsis to be verified.

### Doc rendering

`wado doc` adds a `## Synopsis` section per module, immediately after the
module's `//!` doc and before per-item documentation:

````markdown
## Synopsis

```wado
let p = Point { x: 0, y: 0 };
let q = Point { x: 3, y: 4 };
assert p.distance(&q) == 5.0;
```
````

The code is taken verbatim from the test body, using the source span between
the outermost `{` and `}` of the test block. Indentation is normalised so the
inner code starts at column 0.

A module without any `#[synopsis]` test produces no Synopsis section. Multiple
`#[synopsis]` tests in one module render as successive code blocks under the
single `## Synopsis` heading, in source order.

If a `#[synopsis]` test would fail under `wado test`, `wado doc` still renders
it (so authors can iterate) but emits a warning to stderr. CI is expected to
gate on `wado test` rather than on `wado doc`.

### CLI reporting

The `wado test` summary line gains a synopsis axis, reported separately from
ordinary tests:

```
tests: 42 ok
synopses: 8 ok
```

A failing synopsis is counted on the synopses line, not on the tests line.
This separation matches the spirit of the existing `#[TODO]` axis: different
documentation-vs-verification kinds shouldn't be mixed into a single pass/fail
total.

## Why this design

### Why attribute, not new syntax

`test`, `#[expect_trap]`, `#[TODO]`, and `#[timeout_ms]` already form a
coherent vocabulary: the `test` block is the unit, attributes change its
reporting and pass/fail interpretation. `#[synopsis]` extends the same axis
(it changes the rendering destination, not the structure). Adding a parallel
`synopsis { ... }` keyword would duplicate machinery for a single-bit
distinction.

### Why module-scoped, not item-scoped

The perldoc SYNOPSIS that motivated this WEP is module-level by design. It is
the place a reader looks first to decide whether to read on. Per-item examples
are valuable but secondary, and they introduce binding-by-string-path
fragility. Starting module-scoped keeps the design minimal and matches the
documented goal.

### Why universal `.wado` discovery

The current `*_test.wado` glob is a convenience, not a principle. Synopsis
tests must live in implementation files, so the runner must visit those files.
Compiling no-test files at the same time costs little and surfaces broken
sources earlier — a positive side effect.

### Why no hidden setup

Synopsis quality requires that the visible body be the entire example. A
synopsis whose visible code lies because of hidden preamble is worse than no
synopsis. The body the reader sees is the whole synopsis.

## Consequences

### User-visible

- `#[synopsis]` is additive; existing tests are unchanged.
- `wado test` now visits all `.wado` files under the project root. Files that
  fail to compile but were previously ignored will now produce errors. This is
  intentional.
- `wado doc` output gains a `## Synopsis` section for modules that opt in.
- `*_test.wado` remains the recommended convention for test-only files, but is
  no longer required for discovery.

### Implementation

- `wado-compiler/src/parser.rs`: register `synopsis` as a recognised test
  attribute.
- `wado-compiler/src/resolver/item.rs`, `wado-compiler/src/tir.rs`: add an
  `is_synopsis: bool` field to `TirTest`. Test name generation may prepend a
  `__test_synopsis_` prefix to keep filter behaviour predictable.
- `wado-cli/src/test.rs`: change discovery glob to `**/*.wado`; honour the
  existing ignore rules. Add a synopsis tally to the summary output.
- `wado-cli/src/doc.rs` (or the `wado doc` renderer): add a Synopsis section
  emitter that reads test bodies from source spans and normalises indentation.
- `wado-compiler/tests/fixtures/`: add fixtures covering a module with a named
  synopsis, an unnamed synopsis, multiple synopses, and `#[synopsis]` combined
  with `#[expect_trap]` / `#[TODO]`.

### Trade-offs

- Test discovery cost grows with project size because compile-only files now
  pass through the front end. Parallel compilation mitigates this; benchmarks
  should be revisited if it becomes noticeable on large workspaces.
- Authors may be surprised that `wado test` reports compile errors from
  files that were previously untouched. The CLI help text and the
  `cli-subcommands` doc should call this out.
- A `#[synopsis]` test is, in effect, a public commitment: the example becomes
  documentation. Renaming or changing the documented API now requires updating
  the synopsis. This is the same trade-off Rust doctest authors accept, and
  it is the price of freshness.

## TODOs

- [ ] Add `synopsis` to the recognised test-attribute set in the parser.
- [ ] Add `is_synopsis` to `TirTest` and propagate from resolver to runner.
- [ ] Change `wado test` file discovery to `**/*.wado`.
- [ ] Add a synopsis tally line to the `wado test` summary output.
- [ ] Render `## Synopsis` sections in `wado doc` (markdown, simple, json).
- [ ] Add fixtures: single synopsis, multiple synopses, synopsis with
      `#[expect_trap]`, synopsis with `#[TODO]`.
- [ ] Update `docs/cheatsheet.md` with the `#[synopsis]` form.
- [ ] Update the CLI subcommands doc to describe the discovery change.

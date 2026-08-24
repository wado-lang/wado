# gale-highlight-wado

Syntax highlighter for the Wado language. The parser is generated at build time
by [Gale](../package-gale) from the bundled grammar (`grammar/Wado.g4` +
`grammar/Wado.highlights.scm`); no ANTLR/Gale runtime dependency leaks to
consumers — the runtime is inlined into the generated parser.

## Library

```wado
use { highlight } from "wado-lang:gale-highlight-wado";

let fragment = highlight(&src); // <span class="..."> HTML
```

Public API:

- `highlight(src: &String) -> String` — render to an HTML fragment. Never fails:
  unparsable regions are highlighted best-effort (Gale's resilient CST).
- `parse(src: &String) -> ParseResult` — Gale's resilient parse result
  (`.ok()`, `.diagnostics`).
- `highlight_result(result: &ParseResult, src: &String) -> String` — render an
  already-parsed result.
- `capture_vocabulary() -> List<String>` — the capture classes this grammar can
  emit, sorted, for `gale-highlight`'s `Theme::unstyled` / `Theme::unknown`.

The output is a bare fragment: bring your own CSS and page shell. Classes use
the tree-sitter capture vocabulary, so any tree-sitter theme applies:

| Class              | Token                                  |
| ------------------ | -------------------------------------- |
| `comment`          | line / block / `__DATA__`              |
| `string`           | strings, template text, `` ` ``        |
| `number`           | int / float literals                   |
| `keyword`          | `fn`, `let`, `if`, …                   |
| `operator`         | `matches`                              |
| `variable`         | identifiers in an interpolation        |
| `constant builtin` | `true` / `false` / `null` / `self`     |

The vocabulary is held to `wado-compiler`'s canonical syntax registries by
`mise run check-highlight-vocab`: every keyword the compiler defines carries
the capture its `KeywordCategory` implies, and nothing else is captured as one.

A dotted capture like `constant.builtin` becomes `class="constant builtin"`.
See [`example/standalone.wado`](./example/standalone.wado) for a full styled
page and a starter theme to copy.

## CLI

```sh
wado run package-gale-highlight-wado \
    --output-dir build/highlight example/hello.wado example/json.wado
```

Writes the HTML fragment for each input to `build/highlight/<path>.html`.

## Layout

```
grammar/
  Wado.g4               grammar (the single source of truth for Wado's syntax)
  Wado.highlights.scm   capture query (tree-sitter .scm subset)
src/
  lib.wado              re-exports the generated parse/highlight API
  main.wado             CLI (delegates to gale-highlight's run_cli)
  lib_test.wado         highlight regression tests
tools/
  corpus.wado           shared: the path list and reading a file
  corpus_check.wado     parse verdicts, for `mise run check-grammar`
  highlight_dump.wado   capture spans, for `mise run check-highlight`
example/
  standalone.wado       styled full-page demo / CSS-class reference
```

## Held to the compiler

Two checks keep this package from drifting away from `wado-compiler`, which
owns Wado's syntax:

- `mise run check-grammar` — the grammar must accept exactly what the
  compiler's parser accepts, over the stdlib + fixture corpus.
- `mise run check-highlight` — the two must _colour_ that corpus the same.
  Comment / string / number / keyword / constant / operator are decidable
  without name resolution, so any disagreement there fails. Identifier kinds
  are not: the compiler resolves `function` from `variable` and a
  context-free grammar cannot, so those are reported as a capability gap and
  never gated. `mise run check-highlight-vocab` is the cheap first half —
  vocabulary only, no corpus.

## The gale-highlight framework

The grammar-agnostic `run_cli` batch driver lives in
[`package-gale`](../package-gale/src/highlight/) as `gale-highlight`, so future
`gale-highlight-<lang>` packages reuse it: it speaks only `String`, taking a
grammar's `highlight` as a `fn(&String) -> String` value.

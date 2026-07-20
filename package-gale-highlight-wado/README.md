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

The output is a bare fragment: bring your own CSS and page shell. Classes use
the tree-sitter capture vocabulary, so any tree-sitter theme applies:

| Class              | Token                           |
| ------------------ | ------------------------------- |
| `comment`          | line / block / `__DATA__`       |
| `string`           | strings, template text, `` ` `` |
| `number`           | int / float literals            |
| `keyword`          | `fn`, `let`, `if`, `self`, …    |
| `variable`         | identifiers in an interpolation |
| `constant builtin` | `true` / `false` / `null`       |

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
example/
  standalone.wado       styled full-page demo / CSS-class reference
```

## The gale-highlight framework

The grammar-agnostic `run_cli` batch driver lives in
[`package-gale`](../package-gale/src/highlight/) as `gale-highlight`, so future
`gale-highlight-<lang>` packages reuse it: it speaks only `String`, taking a
grammar's `highlight` as a `fn(&String) -> String` value.

# gale-highlight-wado

Syntax highlighter for the Wado language. The parser is generated at build time
by [Gale](../package-gale) from the bundled grammar (`grammar/Wado.g4` +
`grammar/Wado.highlights.scm`); no ANTLR/Gale runtime dependency leaks to
consumers — the runtime is inlined into the generated parser.

## Library

```wado
use { highlight, document, Theme } from "wado-lang:gale-highlight-wado";

let fragment = highlight(&src);                 // <span class="..."> HTML
let page = document(&fragment, &"a.wado", Theme::Dark);  // standalone page
```

Public API:

- `highlight(src: &String) -> String` — render to an HTML fragment. Never fails:
  unparsable regions are highlighted best-effort (Gale's resilient CST).
- `parse(src: &String) -> ParseResult` — Gale's resilient parse result
  (`.ok()`, `.diagnostics`).
- `highlight_result(result: &ParseResult, src: &String) -> String` — render an
  already-parsed result.
- `document` / `Theme` — themed standalone page (re-exported from
  `gale-highlight` in package-gale).

Classes use the tree-sitter capture vocabulary (`comment`, `string`, `keyword`,
`variable`, `constant.builtin` -> `class="constant builtin"`, …), so any
tree-sitter theme applies. Two built-in themes ship in `gale-highlight`
(`Theme::Light` / `Theme::Dark`).

## CLI

```sh
wado run package-gale-highlight-wado \
    --output-dir build/highlight example/hello.wado example/json.wado
```

Writes `build/highlight/<path>.html` per input. `--standalone`
(with `--theme light|dark`) emits full themed pages instead of fragments.

## Layout

```
grammar/
  Wado.g4               grammar (the single source of truth for Wado's syntax)
  Wado.highlights.scm   capture query (tree-sitter .scm subset)
src/
  lib.wado              re-exports the generated parse/highlight API
  main.wado             CLI (delegates to gale-highlight's run_cli)
  lib_test.wado         highlight regression tests
```

## The gale-highlight framework

The grammar-agnostic pieces — `Theme`, `document`, and the `run_cli` batch
driver — live in [`package-gale`](../package-gale/src/highlight/) as
`gale-highlight`, so future `gale-highlight-<lang>` packages reuse them: they
speak only `String`, taking a grammar's `highlight` as a `fn(&String) -> String`
value.

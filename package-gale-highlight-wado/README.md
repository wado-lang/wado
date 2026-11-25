# gale-highlight-wado

Syntax highlighter for the Wado language. The parser is generated at build time
by [Gale](../package-gale) from [`wado-lang:wado`](../package-wado)'s grammar
and the bundled highlight query (`assets/grammar/Wado.highlights.scm`); no
ANTLR/Gale runtime dependency leaks to consumers — the runtime is inlined into
the generated parser.
[WEP: Gale Highlight Query](../docs/wep-2026-07-12-gale-highlight-query.md)
holds the design of the highlight queries.

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

| Class              | Token                                        |
| ------------------ | -------------------------------------------- |
| `comment`          | line / block / `__DATA__` / format specifier |
| `string`           | strings, template text, `` ` ``              |
| `number`           | int / float literals                         |
| `keyword`          | `fn`, `let`, `if`, …                         |
| `operator`         | `matches`, `+`, `==`, `->`, …                |
| `type`             | type references and type parameters          |
| `property`         | `.field`, literal and pattern field names    |
| `function`         | a template tag, `` sql`…` ``                 |
| `function method`  | `.method()`                                  |
| `variable`         | a contextual keyword used as a name          |
| `constant builtin` | `true` / `false` / `null` / `self`           |

A plain identifier stays uncoloured: telling a function from a variable takes
name resolution, which no context-free grammar has.

A `::` segment naming an identifier stays uncoloured for the same reason.
`Option::None` and `Foo::new` are one shape to the grammar, and the call's `(`
sits outside the path rule, so it cannot tell a variant case from a static
method. A segment spelled with a contextual keyword is a name all the same, so
those words are captured there: `Instant::from(x)` is the shape every `From`
impl is called through. `.method()` is not the same case: there the grammar
matches the `(` alongside the name, so the call is certain. A template tag is
certain for the same reason — the backtick follows the name — so the tag takes
`function` while the owner of a qualified one stays a `::` head.

A dotted capture like `constant.builtin` becomes `class="constant builtin"`.
See [`example/standalone.wado`](./example/standalone.wado) for a full styled
page and a starter theme to copy.

## CLI

```sh
wado run package-gale-highlight-wado \
    --output-dir build/highlight example/hello.wado example/json.wado
```

Writes the HTML fragment for each input to `build/highlight/<path>.html`.

## Held to the compiler

Checks hold the highlight query to what `wado-compiler` colours, and
`wado-lang:wado` holds the grammar to what it parses.

## The gale-highlight framework

The grammar-agnostic `run_cli` batch driver lives in
[`package-gale`](../package-gale/src/highlight/) as `gale-highlight`, so future
`gale-highlight-<lang>` packages reuse it: it speaks only `String`, taking a
grammar's `highlight` as a `fn(&String) -> String` value.

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

| Class              | Token                                        |
| ------------------ | -------------------------------------------- |
| `comment`          | line / block / `__DATA__` / format specifier |
| `string`           | strings, template text, `` ` ``              |
| `number`           | int / float literals                         |
| `keyword`          | `fn`, `let`, `if`, …                         |
| `operator`         | `matches`, `+`, `==`, `->`, …                |
| `type`             | type references and type parameters          |
| `property`         | `.field`, literal and pattern field names    |
| `function method`  | `.method()`                                  |
| `variable`         | `stores[p]`, a contextual keyword as a name  |
| `constant builtin` | `true` / `false` / `null` / `self`           |

A plain identifier stays uncoloured: telling a function from a variable takes
name resolution, which no context-free grammar has.

A `::` segment naming an identifier stays uncoloured for the same reason.
`Option::None` and `Foo::new` are one shape to the grammar, and the call's `(`
sits outside the path rule, so it cannot tell a variant case from a static
method. A segment spelled with a contextual keyword is a name all the same, so those
words are captured there: `Instant::from(x)` is the shape every `From` impl is
called through. `.method()` is not the same case: there the grammar matches the
`(` alongside the name, so the call is certain.

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

`wado-compiler` owns Wado's syntax; three checks keep this package on it:

- `mise run check-grammar` — the grammar must accept exactly what the
  compiler's parser accepts, over the stdlib + fixture corpus.
- `mise run check-highlight` — the two must _colour_ that corpus the same.
  Every class but the identifier kinds is decidable without name resolution
  and fails on disagreement; those are reported as a capability gap instead.
- `mise run check-highlight-vocab` — the vocabulary half of that check, over
  the registries rather than a corpus, so it runs in milliseconds: every
  keyword carries the capture its `KeywordCategory` implies, and nothing else
  is captured as one.

## The gale-highlight framework

The grammar-agnostic `run_cli` batch driver lives in
[`package-gale`](../package-gale/src/highlight/) as `gale-highlight`, so future
`gale-highlight-<lang>` packages reuse it: it speaks only `String`, taking a
grammar's `highlight` as a `fn(&String) -> String` value.

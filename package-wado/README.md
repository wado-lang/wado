# wado

The Wado language as a package: `assets/grammar/Wado.g4`, the grammar of
Wado's whole syntax, and a parser [Gale](../package-gale) generates from it at
build time. The Gale runtime is inlined into the generated parser, so none of it
leaks to consumers.

## Library

```wado
use { parse } from "wado-lang:wado";

let result = parse(&src);
assert result.ok();
```

- `parse(src) -> ParseResult` — Gale's resilient parse result (`.ok()`,
  `.diagnostics`).

## The Grammar

The package exports `assets/grammar/Wado.g4`, so a Kiln clause in another
package can generate its own parser from it, with its own options and inputs:

```wado
use { highlight } from "wado-lang:wado/assets/grammar/Wado.g4"
    with {
        generator: {
            module: "wado-lang:gale",
            inputs: [{ path: "../assets/grammar/Wado.highlights.scm", type: "tree-sitter-highlights" }],
        },
    };
```

[`package-gale-highlight-wado`](../package-gale-highlight-wado) is built this
way.

## Held to the Compiler

`Wado.g4` states Wado's whole syntax, and `mise run check-grammar` holds it to
what `wado-compiler` parses.

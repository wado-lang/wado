# gale-rust

A Rust parser for Wado programs: the
[grammars-v4](https://github.com/antlr/grammars-v4/tree/master/rust) Rust
grammar, built by [Gale](../package-gale/README.md), with the base classes its
predicates call ported to Wado. Every `.rs` file in this repository parses
clean with it.

```wado
use { parse_rust, token_kind_name } from "wado-lang:gale-rust";

let result = parse_rust("fn main() {}");
assert result.diagnostics.is_empty();
```

`parse_rust` returns the generated parser's result: a flat concrete syntax
tree (`result.cst`), the tokens (`result.tokens`), and the diagnostics.
`tree_hash` folds a tree into one number, so two parses can be compared.
`token_kind_name` and the `RK_*` / `TK_*` constants the package re-exports name
the node and token kinds.

## Tools

- `tools/corpus_check.wado` parses a list of Rust files and reports the ones
  that fail.
- `tools/inline_paths.wado` lists the `crate::` and `super::` paths a file
  writes outside a module-level `use`; `mise run check-rust-paths` runs it.
- `benchmark/parse.wado` times the parser on `benchmark/input.rs`.

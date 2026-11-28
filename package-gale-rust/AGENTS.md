# gale-rust Development Guide

Rust as a package: the grammars-v4 Rust grammar, the base classes its
predicates call, ported to Wado, and the parser Gale builds from them. The
`wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md).

- [`docs/corpus-check.md`](./docs/corpus-check.md) — parsing every `.rs` file
  in the repository, the measure of a grammar or prediction change.

## Architecture

The library holds the one Kiln clause that generates the parser from the
grammar, and installs the base ports around every parse. A consumer imports
`wado-lang:gale-rust`; a second clause on the grammar in another package is an
error.

Gale tests itself on this grammar too, reading the `.g4` files by path: its
front end parses them, `benchmark/gale_gen` times the generator on them, and
the oracle self-test runs the lexer with its Java base. A Gale change runs
`wado test package-gale-rust` as well as `wado test package-gale`.

A change to the grammar from upstream is marked `LOCAL:` beside the rule, with
the reason.

## The Base Ports

The Java base classes under `tools/` are the twins of the Wado ports, so
`antlr4_oracle.wado --super` runs ANTLR4 against the base class Gale runs
against. Edit both or neither.

## Running tests

```sh
wado test package-gale-rust
```

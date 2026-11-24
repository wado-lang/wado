# wado Development Guide

The Wado language as a package: `Wado.g4`, the grammar of Wado's whole syntax,
and the parser Gale builds from it. It is to grow into Wado's self-hosted
compiler. The `wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md).

## Architecture

Gale generates the parser at build time from `assets/grammar/Wado.g4`, which
the package also exports for another package's Kiln clause to name.
`package-gale-highlight-wado` builds its highlighter that way.

## Held to the Compiler

`wado-compiler` owns Wado's syntax, and `assets/grammar/Wado.g4` is the single
source of truth for it outside the compiler. A syntax change in the compiler
changes the grammar in the same change.

`mise run check-grammar` holds the grammar to accept exactly what the
compiler's parser accepts, over the stdlib + fixture corpus. It may accept more
only where the parser's error is a `ParseErrorKind::Restriction`, a rule no
context-free grammar states, such as a nesting limit.
`package-gale-highlight-wado` holds the colouring to the compiler's.

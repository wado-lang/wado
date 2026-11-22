# gale-highlight-wado Development Guide

A complete `Wado.g4`, and a syntax highlighter for Wado built from it with
Gale. The `wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md).

## Architecture

Gale generates the parser at build time from `assets/grammar/Wado.g4` and the
highlight query `assets/grammar/Wado.highlights.scm`. The library highlights
with it, and the CLI is `gale-highlight`'s `run_cli` over that function.

## Held to the Compiler

`wado-compiler` owns Wado's syntax, and `assets/grammar/Wado.g4` is the single
source of truth for it outside the compiler. A syntax change in the compiler
changes the grammar in the same change. Three checks keep the two together:

- `mise run check-grammar` — the grammar must accept exactly what the
  compiler's parser accepts, over the stdlib + fixture corpus. It may accept
  more only where the parser's error is a `ParseErrorKind::Restriction`, a rule
  no context-free grammar states, such as a nesting limit.
- `mise run check-highlight` — the two must _colour_ that corpus the same.
  Every class but the identifier kinds is decidable without name resolution
  and fails on disagreement; those are reported as a capability gap instead.
- `mise run check-highlight-vocab` — the vocabulary half of that check, over
  the registries rather than a corpus, so it runs in milliseconds: every
  keyword carries the capture its `KeywordCategory` implies, nothing else is
  captured as one, and every keyword the parser takes as a name is a name in
  the grammar too. That last one a corpus cannot report: a file the grammar
  rejects is dropped from the comparison rather than compared.

# gale-highlight-wado Development Guide

A syntax highlighter for Wado, built with Gale from `wado-lang:wado`'s
`Wado.g4`. The `wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md), and the grammar's in
[`package-wado/AGENTS.md`](../package-wado/AGENTS.md).

## Architecture

Gale generates the parser at build time from `wado-lang:wado`'s exported
`assets/grammar/Wado.g4` and this package's highlight query
`assets/grammar/Wado.highlights.scm`. The library highlights with it, and the
CLI is `gale-highlight`'s `run_cli` over that function.

## Held to the Compiler

The highlight query must colour Wado as `wado-compiler` does. Two checks keep
the two together:

- `mise run check-highlight` — the two must _colour_ the stdlib + fixture
  corpus the same. Every class but the identifier kinds is decidable without
  name resolution and fails on disagreement; those are reported as a capability
  gap instead.
- `mise run check-highlight-vocab` — the vocabulary half of that check, over
  the registries rather than a corpus, so it runs in milliseconds: every
  keyword carries the capture its `KeywordCategory` implies, nothing else is
  captured as one, and every keyword the parser takes as a name is a name in
  the grammar too. That last one a corpus cannot report: a file the grammar
  rejects is dropped from the comparison rather than compared.

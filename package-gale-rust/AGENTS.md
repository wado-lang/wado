# gale-rust Development Guide

Rust as a package: the grammars-v4 Rust grammar, the base classes its
predicates call, ported to Wado, and the parser Gale builds from them. The
`wado-package-standard` skill says where each kind of file goes.
[README.md](./README.md) is the user manual; Gale's own rules are in
[`package-gale/AGENTS.md`](../package-gale/AGENTS.md).

## Architecture

`src/lib.wado` holds the one Kiln clause that generates the parser from
`assets/grammar/`, and installs the base ports around every parse. A consumer
imports `wado-lang:gale-rust`; a second clause on the grammar in another
package is an error.

Gale tests itself on this grammar too, reading the `.g4` files by path: its
front end parses them, `benchmark/gale_gen` times the generator on them, and
the oracle self-test runs the lexer with its Java base. A Gale change runs
`wado test package-gale-rust` as well as `wado test package-gale`.

## The Base Ports

`tools/java/RustLexerBase.java` and `RustParserBase.java` are the Java twins of
the ports in `src/lib.wado`, so `antlr4_oracle.wado --super` runs ANTLR4
against the base class Gale runs against. Edit both or neither.

## The Corpus Check

`tools/corpus_check.wado` parses a list of Rust files and prints one `ok` /
`ng` line each — path, diagnostic count, line, message, and the first
diagnostic's rule stack — plus a summary on stderr. The stack is the part that
carries information: recovery restarts at an item boundary, so most failures
report the same `expected 'extern'` whatever went wrong inside, and only the
stack names the rule.

```sh
git ls-files '*.rs' > target/rs-corpus.txt
wado run package-gale-rust/tools/corpus_check.wado -- --paths-from target/rs-corpus.txt
```

`--paths-from` reads one path per line, the only form that survives a path
with a space.

`--tree-hash` appends a hash of each file's tree to its line. Two runs whose
outputs are identical built the same trees, which is the check for a change
that must not move a prediction.

Every `.rs` this repository tracks parses clean, so any `ng` line is a
regression. Each failing file reports exactly one diagnostic — it dies once and
recovery carries the rest — so the count is files, not errors.

`tools/inline_paths.wado`, which `mise run check-rust-paths` runs, is the
parser's second consumer. It needs the same clean corpus, since a file it
cannot parse is reported rather than counted.

## Running tests

```sh
wado test package-gale-rust
```

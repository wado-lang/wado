# Windrow Development Guide

Windrow finds code that repeats one shape, read off a Gale grammar's tree. The
`wado-package-standard` skill says where each kind of file goes.

- [`README.md`](./README.md) — the user manual.

## Architecture

A language adapter parses a file with its Gale parser and copies the CST into
`Tree`, which names no grammar's types: each generated parser inlines its own
runtime, so two grammars' `CstStore`s are different types. The adapter also
sorts each token kind into a `TokenClass` by its name. `shapes` hashes every
node's subtree with names and literals abstracted, and `find_clones` groups the
nodes of a set of trees by that hash.

A grammar comes from the package that exports it, with the one Kiln clause that
generates it: a second clause on the same file is an error. Rust's is
`wado-lang:gale/src/rust.wado`.

## Running tests

```sh
wado test package-windrow
```

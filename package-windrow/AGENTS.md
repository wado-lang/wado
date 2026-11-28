# Windrow Development Guide

Windrow finds code that repeats one shape, read off a Gale grammar's tree. The
`wado-package-standard` skill says where each kind of file goes.

- [`README.md`](./README.md) — the user manual.

## Architecture

A language adapter parses a file with its Gale parser and copies the CST into
`Tree`, which names no grammar's types: each generated parser inlines its own
runtime, so two grammars' `CstStore`s are different types. The adapter also
sorts each token kind into a `TokenClass` by its name. `shapes` reads each
node's token range and each token as its shape keeps it. `find_similar` gathers
a MinHash signature of every node's set of token runs bottom-up, pairs the
nodes banding puts together (locality-sensitive hashing), and joins the pairs
alike enough into groups whose every two members are.

A grammar comes from the package that exports it, with the one Kiln clause that
generates it: a second clause on the same file is an error. Rust's is
`wado-lang:gale/src/rust.wado`.

## Running tests

```sh
wado test package-windrow
```

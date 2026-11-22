# The Standard Package Layout

Every package keeps each kind of file in the same place, so a reader, a tool or
an agent finds it without learning the package first.

## The Root

The root holds these files:

| File        | Holds                                                                                          |
| ----------- | ---------------------------------------------------------------------------------------------- |
| `wado.toml` | the manifest ([The Manifest](./spec-packages.md#the-manifest))                                 |
| `wado.lock` | the lock file ([The Lock File](./spec-packages.md#the-lock-file)), where the package has one   |
| `README.md` | what the package is, and its user manual                                                       |
| `AGENTS.md` | what to keep in mind on every change to the package                                            |
| `TODO.md`   | the work not done yet, where there is any                                                      |

`README.md` and `AGENTS.md` are required. `TODO.md` is not a document of
`docs/`: a consumer reads it to learn what the package cannot do yet.

## Directories

| Directory    | Holds                                                                  |
| ------------ | ---------------------------------------------------------------------- |
| `src/`       | the package's Wado modules                                             |
| `tests/`     | its tests                                                              |
| `docs/`      | every other document                                                   |
| `assets/`    | the files it bundles that are not Wado modules                         |
| `tools/`     | the programs that maintain it: generators, oracles, corpus checks      |
| `example/`   | programs that show it in use                                           |
| `benchmark/` | its benchmarks                                                         |
| `build/`     | Kiln's intermediates, which the compiler writes and git ignores        |
| `pgo/`       | the profile `wado test --pgo` records ([PGO](./spec-pgo.md))           |

Each directory is there only where it has content. A directory these do not
name may hold what none of them fits, and its package's `AGENTS.md` says what
it holds.

## Documents

`README.md` is where a reader starts. It says what the package does and how to
use it, and links the document that holds the design: `docs/design.md`, or the
WEP that proposed the package. Every other document in `docs/` is named for its
topic.

A package's documents are its own record. Once the WEP that proposed the
package is complete, nothing edits the WEP, and a design decided after it is
written in the package's documents.

## `AGENTS.md`

`AGENTS.md` holds:

- a paragraph saying what the package is;
- what holds only for this package, such as the sources it may read and, for
  each oracle, how to run it and keep it current;
- the package's main components, at the level of its architecture, such as
  Gale's compiler pipeline.

It lists no source files. A file list goes stale with every move, and the
components it names say where to look.

## Tests

A test goes in `tests/`, in a file named `<topic>_test.wado`. A `test` block
stays in a source file for two reasons only:

- It tests a file-private item, which no other file can reach
  ([Visibility](./spec-modules.md#visibility)).
- It is a `#[synopsis]` test, which `wado doc` renders as its module's synopsis
  ([`#[synopsis]`](./spec-attributes.md#synopsis)).

A corpus, a generated test suite, or a package that consumes this one goes in a
directory of its own under `tests/`.

## Assets

`assets/` holds every file the package bundles that is not a Wado module: a
grammar a Kiln import reads, a schema, a WIT file. A directory under it groups
files of one kind, such as `assets/grammar/`.

> Not yet implemented: bundling a file of `assets/` for a consumer to name.
> `exports` lists Wado modules only ([Exported Files](./spec-packages.md#exported-files)).

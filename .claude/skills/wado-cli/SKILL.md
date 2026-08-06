---
name: wado-cli
description: How to drive the `wado` command — the subcommand list, and the behaviour its `--help` does not state (directory grants, reading a test run, symbol notation, inspecting invalid Wasm). Read before invoking the `wado` binary.
---

# The `wado` CLI

`wado <command> --help` is the source of truth for flags, and it is thorough —
it states the allocator modes and their defaults per world, the optimization
levels, every `dump` phase, every `query` kind, and how `publish` authenticates.
Run it rather than guessing. This file carries the command list and the things
that help text does not say.

Inside the wado repository, `wado` means `cargo run --bin wado --`.

## Commands

```
Usage: wado <command> [options]

Commands:
  init [options]                      Create a new wado.toml manifest
  update [options]                    Resolve dependencies and write wado.lock
  fetch [options]                     Download the project's registry dependencies
  clean [options]                     Evict derived cache state (git worktrees)
  build [options]                     Build the project's worlds from wado.toml
  compile [options] <file.wado>       Compile a single Wado source file
  check [options] [file.wado]         Verify a source file and its Kiln generators
  run [options] [file.wado]           Compile and run a Wado CLI program
  serve [options] [file.wado]         Compile and serve a Wado HTTP service
  test [options] [files or dirs...]   Run tests in Wado source files
  format [options] <file.wado>...     Format a Wado source file
  doc [options] <file.wado>...        Generate documentation from source files
  dump [options] <file.wado>...       Dump compiler internal state
  wit [options] [file.wado | dir]     Emit the WIT contract for a Wado program
  syntax [options]                    Generate syntax definition files
  lsp [options]                       Start the language server (LSP over stdio)
  query <kind> [options] <file.wado>  Query language service information
  publish [options]                   Check whether the package can be published

Global options:
  --help     Show this help message
  --version  Show version information
```

`build` works from a `wado.toml` and writes `build/<world>.wasm`; `compile`,
`run`, `serve`, and `dump` take a single source file.

## What `--help` does not say

### Directory grants

A program reaches only the directories granted to it — the current one by
default, or exactly the `--dir` grants once any is given. Paths open relative to
a grant, so **an absolute path never opens**. Reach a file outside the tree by
granting its directory and naming the file relative to that:

```sh
wado run --dir /tmp/scratch prog.wado Foo.g4  # Foo.g4 resolves inside /tmp/scratch
```

### Reading a test run

A failure or a resolved `#[TODO]` prints its own one-line notice immediately.
Otherwise `wado test` prints a digest every 5s —
`N/Total files · tests, failed, todo, skip · ETA` — and ends with a
`compile:`/`load:`/`skip:`/`test:` summary. Reading the last line or two is
enough to know where a long run stands.

### Inspecting invalid Wasm

When codegen emits a module the validator rejects, combine the two flags to see
the WAT anyway:

```sh
wado compile --no-validate --wat-to-stdout file.wado
```

### Symbol notation

`--symbol` takes `MODULE#SYMBOL`, which `--help` only illustrates:

- `MODULE` is the import specifier, quoted as in `use` — droppable for a scheme or bare name (`core:json`), required for a path or URL (`"./utils.wado"`).
- `SYMBOL` uses Wado's own operators: bare `name` (free function/type/global), `Type::name` (associated const/fn), `Type.name` (method), `Type^Trait::name` (trait-impl member).

```sh
wado query hover --symbol core:json#from_string
wado query definition --symbol core:cbor#CborDeserializer.peek_byte
wado query references --symbol core:cli#println --base example
```

`references` loads every `.wado` under `--base`, so it spans the workspace
rather than one file. For a type, `hover` also lists its `impl` blocks. The
notation spec is `docs/wep-2026-06-14-symbol-notation.md`.

### Formatting in this repository

`mise run format-wado` formats every fixture the compiler tests use — including
uncommitted ones, which it may not leave intact.

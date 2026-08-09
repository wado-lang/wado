---
name: wado-cli
description: How to drive the `wado` command — compile, run, test, serve, format, and publish a Wado program, target a Wasm world, pick an allocator, grant directories, and inspect the compiler with dump and query. Read before invoking the `wado` binary.
---

# The `wado` CLI

`wado <command> --help` is the source of truth for flags and is thorough — it
states the allocator modes and their per-world defaults, the optimization
levels, every `dump` phase, and every `query` kind. Run it rather than guessing.

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

## Target World

A Wado program targets a Wasm _world_: the CLI command (`wasi:cli/command`, the
default), the HTTP service (`wasi:http/service`, run via `wado serve`), or the
synthetic test world (selected with `--world test`, used by E2E tests). Several
defaults — including the allocator — depend on the target world.

`--world <name>` overrides it. `--world test` exports the entry module's `test`
blocks and drops everything else. `compile` and `check` accept it; `serve` and
`test` pick their world automatically.

```sh
wado compile --world test file.wado  # compile against the test world
wado check --world test file.wado    # type-check against the test world
```

## Allocators

Three allocators are available via `--allocator <mode>`:

- `bump` (default for CLI): bump pointer; never frees. Fast, minimal code.
- `freelist` (default for the HTTP world): reclaims freed memory via a free list. For long-running processes.
- `debug` (default for the test world): never reuses freed memory; poisons freed memory with `0xFF`. For use-after-free detection.

```sh
wado compile --allocator bump file.wado      # bump allocator
wado compile --allocator freelist file.wado  # free-list allocator
wado compile --allocator debug file.wado     # debug allocator
```

`wado compile` selects the `debug` allocator automatically when targeting the
test world; E2E tests rely on this.

## Compile

```sh
wado compile -o file.wasm file.wado    # generate Wasm
wado compile -o file.wat file.wado     # generate WAT
wado compile --wat-to-stdout file.wado # output WAT to stdout
```

Optimization levels: `-O0` (none), `-O1` (development), `-O2` (production,
default), `-O3` (aggressive), `-Os` (`-O2` + strip symbols).

To inspect invalid Wasm when debugging codegen bugs, skip validation:

```sh
# Output raw Wasm bytes even if invalid
wado compile --no-validate --wat-to-stdout file.wado
```

`wado check` verifies a source file — and re-runs its Kiln generators, comparing
the output against the committed source — without emitting Wasm.

## Run

```sh
wado run file.wado  # run a CLI program with wasmtime
```

A program reaches only the directories granted to it: the current one, or
exactly the `--dir` grants once any is given. Paths open relative to a grant, so
an absolute path never opens — reach a file outside the tree by granting its
directory and naming it relative to that.

```sh
wado run --dir /tmp/scratch prog.wado Foo.g4  # Foo.g4 resolves inside /tmp/scratch
```

## Test

`wado test` discovers and runs `test` blocks (compiled against the `test` world,
see Target World above).

```sh
wado test                           # discover and run every .wado test in the project
wado test file.wado                 # run tests in one file
wado test --filter '**/json*.wado'  # run tests in files matching a wildcard
```

A failure or resolved `#[TODO]` prints its own one-line notice immediately,
otherwise a digest (`N/Total files · tests, failed, todo, skip · ETA`) prints
every 5s, ending in a `compile:`/`load:`/`skip:`/`test:` summary. `tail`ing the
last line or two is enough to read the current state of a long run.

## Serve

Use `wado serve` to run a Wado HTTP service (wasi:http/service world):

```sh
wado serve file.wado                        # serve on 0.0.0.0:8080 (default)
wado serve --addr 127.0.0.1:3000 file.wado  # serve on a custom address
```

## Dump

Use `wado dump` to inspect compiler internal state for debugging.
See `wado dump --help` for the full help.

```sh
wado dump file.wado                  # show final WIR (default)
wado dump --nir file.wado            # show final NIR (after optimization)
wado dump --nir -O0 file.wado        # show NIR without optimization
wado dump --ast file.wado            # show parsed AST
wado dump --modules file.wado        # show loaded modules
wado dump --symbols file.wado        # show symbol table
wado dump --types file.wado          # show type table
wado dump --tir-resolved file.wado       # show TIR after type resolution
wado dump --tir-monomorphized file.wado  # show TIR after monomorphization
wado dump --nir-lowered file.wado        # show NIR right after lowering (before optimize)
```

## Query

`wado query` answers compiler questions about a symbol, for tooling and docs. A
symbol is addressed either by position (`--line`/`--column` in a file) or by
_symbol notation_ `MODULE#SYMBOL`:

- `MODULE` is the import specifier; quote it as in `use` — droppable for a scheme or bare name (`core:json`), required for a path or URL (`"./utils.wado"`).
- `SYMBOL` uses Wado's operators: bare `name` (free function/type/global), `Type::name` (associated const/fn), `Type.name` (method), `Type^Trait::name` (trait-impl member).

```sh
wado query hover --symbol core:json#from_string                   # signature / type
wado query hover --symbol ./hello.wado#run --base example          # local module
wado query definition --symbol core:cbor#CborDeserializer.peek_byte
wado query references --symbol core:cli#println --base example     # all uses (workspace)
wado query hover --line 5 --column 10 file.wado                   # position-based
wado query diagnostics file.wado                                  # errors/warnings
```

Common options:

- `--symbol <notation>` — locate by name instead of `--line`/`--column`.
- `--base <dir>` — anchor relative modules (default: cwd; `core:` / `wasi:` are location-independent).
- `--all` — include private members; the default is the public-API view (matches `wado doc`).
- `--json` — machine-readable output.

For a type, `hover` also lists its `impl` blocks. `references` loads every
`.wado` under `--base`, so it spans the workspace. See
`docs/wep-2026-06-14-symbol-notation.md` for the notation spec.

## Format

The `wado format` command formats Wado source code.

```sh
wado format -w file.wado  # rewrite in place
```

In the wado repository, `mise run format-wado` formats all the fixtures used by
compiler tests.

**Caution:** `mise run format-wado` may break uncommitted test fixtures. When the
syntax is updated, make sure to add tests to `wado-compiler/tests/format.rs`.

## Publish

`wado publish` builds the package and uploads it through `wkg`. Credentials
belong to `wkg`, not Wado — authenticate to the registry first (`docker login`,
or `WKG_OCI_USERNAME` / `WKG_OCI_PASSWORD`; for GHCR the password is a token with
the `write:packages` scope). `--dry-run` runs every readiness check without
uploading.

## Compilation Log and Timing

The compiler emits timestamped diagnostics to stderr. Use `--log-level` to
control verbosity (`debug`, `info`, `warn` — the default — `error`, `off`).

```sh
wado compile --log-level debug file.wado
```

## Optimizer Remarks

A `remark:` reports a cost the optimizer could not remove, at the exact source
span. They are info-level, so the default `warn` hides them — ask for them when
you are chasing why something is slower or larger than expected, not on every
build.

```sh
wado check --log-level info file.wado              # no Wasm emitted; fastest
wado check --world test --log-level info lib.wado  # a library with test blocks
```

Two kinds are reported, both only for the entry module — stdlib internals would
drown out your own code:

- A **value-semantic copy that survived**. Wado deep-copies aggregates on
  assignment, argument passing, and return; most are optimized away, and the
  ones left are invisible while reading the source.

  ```
  file.wado:6:5: info: remark: a copy of `List<i32>` survives optimization
  ```

- A **compile-time parameter that still decides a branch**. A `#[param]` global
  exists to be settled at build time, so a surviving read means the build kept a
  run-time test it was told to decide — `-D log.level=info` not stripping what it
  was supposed to strip. The remark names the parameter, and the intermediate
  global when the gate reads a derived one rather than the parameter itself.

  ```
  file.wado:111:5: info: remark: compile-time parameter `log.level` is still read
  here through global `LOG_STATIC_LEVEL`, so this branch is decided at run time;
  the code it guards was not stripped
  ```

Design: `docs/wep-2026-06-03-optimizer-remarks.md`. What is not yet removable —
and so what a remark is currently expected to report — is listed under "Not yet
implemented" in `docs/optimizer.md`.

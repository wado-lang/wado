# wado-cli

The `wado` binary: a subcommand-style CLI over `wado-compiler` and `wado-lsp`.

In the examples below, `wado` is shorthand for `cargo run --bin wado --`.
`wado --help` lists every subcommand; the sections below cover the ones with
workflows worth writing down.

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

- `bump` (default for CLI): Bump pointer; never frees. Fast, minimal code.
- `freelist` (default for HTTP world): Reclaims freed memory via a free list. For long-running processes.
- `debug` (default for test world): Never reuses freed memory; poisons freed memory with `0xFF`. For use-after-free detection.

```sh
wado compile --allocator bump file.wado      # bump allocator
wado compile --allocator freelist file.wado  # free-list allocator
wado compile --allocator debug file.wado     # debug allocator
```

`wado compile` selects the `debug` allocator automatically when targeting the
test world; E2E tests rely on this.

## Compile Command

```sh
wado compile -o file.wasm file.wado    # generate Wasm
wado compile -o file.wat file.wado     # generate WAT
wado compile --wat-to-stdout file.wado # output WAT to stdout
```

To inspect invalid Wasm when debugging codegen bugs, use `--no-validate`:

```sh
# Skip validation and output raw Wasm bytes even if invalid
wado compile --no-validate --wat-to-stdout file.wado
```

Optimization levels: `-O0` (none), `-O1` (development), `-O2` (production,
default), `-O3` (aggressive), `-Os` (`-O2` + strip symbols).

## Run Command

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

## Test Command

`wado test` discovers and runs `test` blocks (compiled against the `test` world,
see Target World above).

```sh
wado test                    # discover and run every .wado test in the project
wado test file.wado          # run tests in one file
wado test --filter '**/json*.wado'  # run tests in files matching a wildcard
```

A failure or resolved `#[TODO]` prints its own one-line notice immediately,
otherwise a digest (`N/Total files · tests, failed, todo, skip · ETA`) prints
every 5s, ending in a `compile:`/`load:`/`skip:`/`test:` summary. `tail`ing the
last line or two is enough to read the current state of a long run.

## Serve Command

Use `wado serve` to run a Wado HTTP service (wasi:http/service world):

```sh
wado serve file.wado                        # serve on 0.0.0.0:8080 (default)
wado serve --addr 127.0.0.1:3000 file.wado  # serve on a custom address
```

## Dump Command

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

## Query Command

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

## Format Command

The `wado format` command formats Wado source code.

`mise run format-wado` formats all the fixtures used by compiler tests.

**Caution:** `mise run format-wado` may break uncommitted test fixtures. When the
syntax is updated, make sure to add tests to `wado-compiler/tests/format.rs`.

## Compilation Log and Timing

The compiler emits timestamped diagnostics to stderr. Use `--log-level` to
control verbosity.

```sh
wado compile --log-level debug file.wado
```

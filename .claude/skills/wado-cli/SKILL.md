---
name: wado-cli
description: How to drive the `wado` command — compile, run, test, serve, format, and publish a Wado program, target a Wasm world, pick an allocator, grant directories, and inspect the compiler with dump and query. Read before invoking the `wado` binary.
---

# The `wado` CLI

`wado --help` and `wado <command> --help` are the source of truth for commands
and flags: worlds, allocators, `-O` levels, every `dump` phase and `query` kind.
Inside the repository `wado` means `cargo run --bin wado --`. What follows is
what `--help` does not say.

## External Commands

An unknown command runs `wado-<name>` from `PATH`, with the rest of the command
line unparsed and `$WADO` naming the caller (`wado run-webgpu` is one). Only
absolute `PATH` entries are searched, and a builtin always wins; `wado --list`
shows both.

## Worlds

`run`, `serve` and `test` pick their world. `compile`, `dump` and `wit` default
to `wasi:cli/command`; `--world` overrides it. `build --world <fq>` instead picks
one of `wado.toml`'s worlds.

## Check

`wado check` runs Kiln generators and resolves dependencies as a build does,
then stops before emitting, at `-O0`. With no path, or a directory, it checks
every world the manifest declares. A named file is checked against the world
whose `[world]` entry names it, or else the library world, which needs no entry
point.

## Test

```sh
wado test --filter '**/json*.wado'  # files matching a wildcard
wado test --profile guest f.wado    # see wado-performance
wado test --coverage=lcov,json      # into build/coverage/
```

A failure prints at once; otherwise a digest prints every 5s, so the last line
of the log is the run's state. `--profile` takes one file, runs it serially, and
lifts the per-test timeout. `wado dump --coverage-plan` shows a file's coverage
regions.

## Fuel

`--report-fuel` on `run`, `test` and `serve` counts guest instructions, so pure
computation in one build spends the same on any machine; host calls and GC
spend none, but a guest waiting on I/O loops as often as the host makes it. A
test's count includes instantiation. Under `serve` it forces one worker and one
request at a time, leaves out the query string, marks a request that trapped
its worker `(trapped)`, and reports what a task spends after its response on an
`(after response)` line, until the next request arrives.

## Query

A symbol is addressed by `--line`/`--column` or by `MODULE#SYMBOL`
(`docs/wep-2026-06-14-symbol-notation.md`): the module as written in a `use`,
then `name`, `Type::name`, `Type.name` (a method), or `Type^Trait::name`.

```sh
wado query hover --symbol core:json#from_string
wado query references --symbol core:cli#println --base example
wado query inlay-hints file.wado   # hints spliced into the source
```

`--base` anchors relative modules and bounds what `references` loads; `--all`
adds private members. `inlay-hints` shows each hint at its anchor
(`let x‹: i32› = …`), which is how to check placement; `--json` gives UTF-16
positions.

## Format

Each package skips `**/generated/**`, `**/build/**` and its
`[format] exclude`; `[format] include` opts a path back in. Rules are in
`docs/formatter.md`.

## Publish

`wado publish` uploads through `wkg`, whose credentials it uses (`docker login`,
or `WKG_OCI_USERNAME` / `WKG_OCI_PASSWORD`; a `write:packages` token for GHCR).
`--dry-run` runs every check without uploading.

## Optimizer Remarks

`--log-level info` (the default is `warn`) reports, for the entry package's own
modules, each value copy that survived optimization and each compile-time
parameter that still decides a branch at run time. `wado check --log-level info`
is the fastest way to see them. Design: `docs/wep-2026-06-03-optimizer-remarks.md`.

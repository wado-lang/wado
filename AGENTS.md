# The Wado Programming Language

This document describes how to develop the Wado compiler toolchain.

## Principles

- Succinctly — say and write the least that fully conveys the point.
- Fix-forward — fix the cause of a defect and move forward; never backtrack.
- Fix the class — a defect is one instance; fix what admits the class. An input nobody would supply is bounded by one constant where it enters, not modelled.
- A compiler bug is P0. The instant you suspect one, stop all other work, write a minimal e2e fixture, and fix it. A compiler rule that blocks an edit is a suspect, not an obstacle to route around.
- Design is the user's call. A rule invented to soften a fix's side effect is a design decision: propose it with the trade-off.
- Red/green TDD. A pre-existing issue is fixed too.

## Development

Tools come from [mise](https://mise.jdx.dev/) (`curl -fsSL https://mise.run | sh`); tasks are in `mise.toml`.

```sh
mise trust                 # first time only
mise run on-task-started   # install project tools
```

### The Cycle

Write the change, commit it, run `/distill` over the whole branch, then test
once. Run `/distill` again after answering review findings, which go through
`/code-review-response` however they arrive.

### Common Development Tasks

```sh
mise run test            # check the spec examples, then test Rust crates
mise run test-wado       # test Wado modules
mise run test-stdlib O3  # test the stdlib at one optimization level (CI runs each)
mise run check           # run the corpus checks that finish in seconds
mise run format          # format Rust, Markdown, and Wado files

mise run benchmark-all     # runs all benchmarks and reports the results
mise run report-wasm-size  # measures the size of the generated Wasm files and reports the results
```

Iterate with the `dev` profile (`cargo build` / `run` / `test`); `Cargo.toml`
raises its `opt-level` where it matters. `--release` is for distribution.

## Tooling

- Make every edit with the editing tools.
- Run one heavy job at a time, in the background, its output in a file outside the tree that records its own end: `cmd > /tmp/run.log 2>&1 && s=0 || s=$?; echo "exit=$s" >> /tmp/run.log`. Scratch files never go in the tree, where `git add -A` picks them up. Wait for the harness's notification, never a `sleep` loop. An abandoned job keeps running, so kill it by pid (`ps -eo pid,etime,pcpu,args`) before restarting; `test`, `test-wado`, `test-stdlib` and `test-gale-o2` refuse to start beside themselves where `flock` exists.
- A generated file carries `-diff` in `.gitattributes`: regenerate and commit it, don't read it. `scripts/changed-sources.sh` lists the changed sources without them. On an internal pull request CI's `tidy` job regenerates them, with clippy and format, and pushes `chore: tidy`, so pull before pushing.

## Git

- Commit and push each self-contained unit without asking, never onto `main`. Open a pull request only when the user asks.
- Read the whole diff before committing. A trimmed comment or doc keeps every clause that names a pass, a type, a literal, a condition, or the bug behind the code.
- A defect found mid-task is fixed on the branch in hand.
- Delete local branches `git branch --merged main` lists without asking, except `main` and a branch with no commits of its own yet; never one `--no-merged` lists.
- Merge `origin/main` only through the `git-upstream-sync` skill. CI's test jobs run on the branch merged with `main`, so reproduce a CI-only failure there first. `tidy` is the exception: it checks out the head.

## Writing

- English, plain. A comment says only why; an invariant is an assert; every `pub` item has a doc comment saying what it is. Markdown follows the `markdown` skill.
- An issue elsewhere is `org/repo#num`; a bare `#num` is this repository.
- A `crate::` or `super::` path goes in a `use` at the top, never inline (`pub(in …)` is exempt). `mise run check-rust-paths` checks it.
- No wall-clock seconds in comments or docs: this machine is the fastest one. Write the ratio.

## Testing

- Test the language from an e2e fixture: a `.wado` file in `wado-compiler/tests/fixtures/`, expectations in its `__DATA__`. Nearly everything the language does is stated there, diagnostics included. `{"compile_error": "…"}` matches the whole report, position included; `{"compile_error_codes": [...]}` names the `Code`. Kiln is the exception, since a generator needs the filesystem.
- Test a stdlib function from the `*_test.wado` beside its module.
- Write an integration test only for what no fixture can state, in `tests/integration/`, declared in its `main.rs`. Each file directly in `tests/` links another ~150 MB target.
- A regression fixture's shape is its point: when a harness cannot classify one, fix the harness.
- Run what the change can reach, not more.
- An unexplained failure indicts the measurement as often as the change: re-run the exact command by hand before believing it. zsh does not word-split `$VAR`.
- A CI-only timeout in a test reading `wasi:clocks` is runner slowness.

## The Wado Language

Much of the syntax is Rust-compatible by design, so most Rust knowledge carries
over. It stops short in a few places: a value never needs `.clone()`, `enum`,
`variant` and `flags` split what Rust puts in one `enum`, and imports follow ES
modules while tuples follow TypeScript.

@docs/cheatsheet.md is the quick reference. For the detailed specification read
`docs/spec-*.md` (one file per area, indexed in `docs/README.md`), or the WEP
that proposed a feature at `docs/wep-*.md`. `wado doc <module>` (`core:prelude`, say)
states a stdlib module's signatures; read it rather than guess.

Avoid committing a binary: its diff is unreadable. Where a text form means the
same, commit that instead (`.wat` for `.wasm`, `.onnxtext` for `.onnx`).

## Repository Map

- `wado-compiler/` — the compiler: frontend, IR pipeline, optimizer, codegen. The Wado standard library (`core:*`, `wasi:*`) lives in `wado-compiler/lib/`. Internals: `docs/compiler.md`, `docs/optimizer.md`.
- `wado-cli/` — the `wado` binary.
- `wado-run-webgpu/` — the `wado run-webgpu` subcommand, a separate binary and a workspace of its own: it links a GPU stack on a wasmtime other than the pin. The `test-webgpu` CI job is the only one that builds it.
- `wado-lsp/` — the language service engine, also compiled to Wasm for the browser.
- `wado-vscode/` — the VS Code extension.
- `wado-from-idl/` — generates the `wasi:*` and `core:kiln` stdlib modules from WIT, and `package-web`'s DOM bindings from WebIDL.
- `wado-manifest/` — `wado.toml` / `wado.lock` parsing, validation, and dependency resolution.
- `wado-wasm-embed/` — prepares a core wasm asset for embedding in a component: memory definition to import, then a prune to the used exports.
- `wado-bundled-libm/` — deterministic math, bundled into the compiler as a Wasm module.
- `wado-bundled-icu/` — ICU binding for Wado (under development; not bundled yet).
- `docs/` — the language spec (`docs/spec-*.md`), the compiler and formatter guides (`docs/compiler.md`, `docs/optimizer.md`, `docs/formatter.md`), stdlib docs, and the Wado Evolution Proposals (`docs/wep-*.md`).
- `benchmark/`, `wasm-size/` — performance and code-size measurement.
- `cloudflare-worker/` — serves a `wasi:http/service` component from a Cloudflare Worker, via jco.
- `package-gale/` — a parser generator compatible with ANTLR4 (`.g4`) in Wado.
- `package-gale-highlight-wado/` — a complete `Wado.g4` and a syntax highlighter for Wado source code, built with `package-gale`.
- `package-grog/` — a Protocol Buffers compiler in Wado: a `.proto` becomes Wado declarations, and the runtime library encodes them.
- `package-jade/` — a JSON Schema 2020-12 validator in Wado.
- `package-marl/` — a CommonMark subset in Wado.
- `package-loam/` — a tensor compiler in Wado: an ONNX graph becomes Wado source, shapes checked at build time.
- `package-wadopoet/` — builders for generated Wado source, and the reserved vocabulary (generated from `wado syntax --format json`) a minted name must avoid.
- `package-web/` — `wado-lang:web`: the web platform bindings, their browser glue, and `SurfaceDom`, a DOM without a browser engine that serves them under `wado test`, `wado run` and `wado serve`.
- `package-cm-catalog/` — a catalog of Wasm Component Model modules for demo and testing purposes.
- `vendor/` — reference specs and runtimes, as git submodules (`git submodule update --init --recommend-shallow`).

## The CLI

`wado` is `cargo run --bin wado --`. `wado --help` lists the subcommands; the
`wado-cli` skill covers the workflows. Behavior that no `--help` will remind you
of:

- A program targets a Wasm _world_: `wasi:cli/command` (default), `wasi:http/service`, or the synthetic `test` world, which exports the entry module's `test` blocks and nothing else.
- The world selects the allocator: `bump` for CLI (never frees), `freelist` for HTTP, `debug` for tests (never reuses freed memory, poisons it with `0xFF`). E2E tests rely on `debug`.
- `wado run` reaches only the current directory, or exactly the `--dir` grants once any is given. An absolute path never opens.
- The formatter skips `wado-compiler/tests/**`, but only when walking a directory: never `wado format -w` a fixture file. A syntax change adds tests to `wado-compiler/tests/format.rs`.

## Dependencies

The wasm-tools crates (`wasmparser`, `wasm-encoder`, `wasmprinter`, `wit-parser`, `wat`) are pinned in `[workspace.dependencies]` to the generation wasmtime depends on, so cargo dedupes them. `mise run check-deps` enforces this. When bumping wasmtime, find its generation (`cargo tree -i wasmparser@<ver>`), re-pin them (`wat = "~1.<gen>"`, the rest `"0.<gen>"`), then `cargo update` and `mise run check-deps`.

## References

Wado targets Wasm 3.0 (GC and JSPI included), the Component Model, and WASI 0.3
(p3), all fully supported by wasmtime. The sources of truth are vendored:

- Component Model: `vendor/component-model/design/mvp/` (`CanonicalABI.md`, `Concurrency.md` for async, streams and futures)
- WASI p3: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

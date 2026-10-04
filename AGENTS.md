# The Wado Programming Language

This document describes how to develop the Wado compiler toolchain.

## Principles

- Succinctly — say and write the least that fully conveys the point.
- Fix-forward — fix the cause of a defect and move forward; never backtrack.
- Fix the class — a defect is one instance; fix what admits the class.

## Development

This project uses [mise](https://mise.jdx.dev/) to manage dev tools. Project tasks are defined in `mise.toml`. Run `mise tasks` to discover available tasks.

Install mise first if you don't have it:

```sh
curl -fsSL https://mise.run | sh
```

### When Starting a Task

Run the following to set up your development environment:

```sh
mise trust                 # trust the mise.toml config (first time only)
mise run on-task-started   # install project tools
```

### The Cycle

Write the change, commit it, invoke the `/distill` skill, then test. `/distill`
is the last of the editing rather than a phase after it, so one full test run at
the end answers for the change and for what `/distill` edited. A run on either
side of it is the same hour spent twice.

Having invoked `/distill` on this branch an hour ago is not a reason to skip the
next one. The scope is the whole branch every time, and what the commits since
then made stale is spread across everything the branch touched.

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

## Tooling

- Make every edit with the editing tools. They refuse a match that is not unique and a file the session has not read, and the harness tracks what they wrote, so each edit is checkable.
- Run a long job (`mise run test`, `test-wado`, `update-golden-fixtures`) in the background, one per invocation, and stop: the harness announces the end of a job it owns. Never start a second heavy job while one runs, chained or as a separate task; two at once run out of memory or starve each other of CPU.
- `test`, `test-wado`, `test-stdlib` and `test-gale-o2` refuse to start while another of the same is running, naming the holder's pid — where `flock` exists; `scripts/exclusive.sh` says so on stderr and runs unlocked where it does not. Abandoning a job does not stop it, so restarting after an edit means killing that pid first.
- Redirect a job's output to a file and read the file, so what you did not anticipate is still there.
- Have the job record its own completion — `cmd > run.log 2>&1 && s=0 || s=$?; echo "exit=$s" >> run.log` — and read the marker once the harness announces the end. The `&&`/`||` is what writes the marker on a failure too, which `set -e` would otherwise exit before. Never wait with an `until … sleep` loop: it outlives the tool timeout as a background job of its own, and each one re-issued stacks another.
- A run that looks hung is usually starved, not dead. `ps -eo pid,lstart,etime,pcpu,args` shows what else holds the cores, an abandoned run of your own included.
- A generated file carries `-diff` in `.gitattributes`, so `git diff`, `git show` and `git log -p` report it as changed without printing it, and the sources stay readable. Regenerate and commit those; do not read them. `--text` prints one where you do want it. `rg` reads them like any other file; `git grep` calls them binary.
  On an internal pull request the `tidy` job regenerates them, along with clippy and format, and pushes `chore: tidy` onto the branch, so leaving them stale costs nothing. The branch then moves without you: pull it before pushing.
- `git diff <base> -- $(scripts/changed-sources.sh)` narrows further, to the changed paths themselves: a stat line or a rename for a generated or vendored file is gone too.

## Git

- Commit each finished unit and push it, without asking. A unit stands on its own: if splitting two changes would leave a broken commit between them, they are one commit. Never commit onto `main`; branch first.
- Open a pull request only when the user asks for one. Related pieces usually go into one pull request.
- Read the whole `git diff` before committing. Trimming a comment tends to keep the abstract statement and cut the concrete half, which is backwards: a clause that names a pass, a type, a literal, or the bug behind the code stays.
- Fix a defect found during a task on the branch in hand, not on a side branch or worktree.
- Delete the local branches `git branch --merged main` lists without asking, then `git remote prune origin`. A branch checked out in a worktree goes only once that worktree is clean. Never delete one `--no-merged` lists.

## General Rules

- Write all documentation and comments in clear, simple English.
  - Comments: write one only for what the code cannot say, and make it say why: why this way, a tradeoff, a constraint, a spec or bug reference. Make the code say what it can: rename and decompose until the comment is redundant, then delete it.
  - Invariants: state them as assertions, not comments. An assert is checked; a comment goes stale.
  - Doc comments (`///`, `//!`): write one on every `pub` item. Say what the item is, not how it works.
  - Markdown: the `markdown` skill holds the rules. Read it before writing or editing any `.md` file.
  - Issue references: an issue or pull request in another repository is written fully qualified, `org/repo#num` (`antlr/antlr4#4911`). A bare `#num` means this repository.
- Name an item, don't spell out its path: a `crate::` or `super::` path belongs in a `use` item at the top of the module, never inline where the item is read. A `pub(in …)` is exempt: it names a scope rather than reading an item, and Rust admits no import there. `mise run check-rust-paths` gates this in CI. The corpus carries no inline path, so the baseline `scripts/rust-inline-paths.json` is empty and any file that gains one fails. The detector is Wado (`package-gale/tools/rust_inline_paths.wado`) and parses with the Gale Rust grammar, so the grammar decides what counts as a path. `scripts/check-rust-paths.sh <file.rs>…` lists what a file carries.
- Perform red/green TDD.
- A compiler bug is always P0 — no exceptions. The instant you suspect one, stop all other work, and as the top priority write a minimal reproducible e2e fixture and fix it.
- A compiler rule that blocks a stdlib edit is a compiler bug until shown otherwise. Never route around it: widening a modifier until the build passed once shipped a public-API regression, and moving per-instance state into a `global mut` shares it across every instance. Compare the parallel paths; the one that handles the case differently is usually the bug.
- A pre-existing issue must be fixed, with TDD when practical.
- Use plain `cargo build` / `cargo run` / `cargo test` (the `dev` profile) for iteration. `Cargo.toml` raises `opt-level` on `wado-compiler`, `wado-dev-tools`, and deps so dev-build runtime is close to release for the parts that matter. `--release` is only for distributing binaries.
- Merge origin/main only through the `git-upstream-sync` skill, conflicts or not; a clean merge still ends with its sanity check.
- A failure that shows only in CI is usually not the environment: a pull request's test jobs run on the branch merged with `main`, not on the branch head. Sync first with the `git-upstream-sync` skill and reproduce on the merged tree before suspecting anything else. `tidy` is the exception, checking out the head ref.
- A CI-only per-test timeout in a test that reads `wasi:clocks` (`core:temporal_test.wado` most often) is runner slowness, not the branch. `wado test -O2 -p 1 --format tap <file>` times it locally against its budget.
- An unexplained failure points at the measurement as often as at the change. Before reverting or calling it a design problem, re-derive it another way: run the exact command by hand, then suspect shell quoting (zsh does not word-split `$VAR`), a stale binary, a suite that aborted before the crate you care about, and a pipeline whose status came from the wrong stage.
- A change that keeps exposing pre-existing bugs in other passes is fuzzing the compiler, not overreaching. Fix each with its own fixture and commit; never narrow the change to stop finding them.
- The fix is what the finding names. A new rule invented to soften the fix's side effect is a design decision: propose it with the trade-off and let the user pick.
- Bound an input nobody would supply with one constant where it enters, not a parameter threaded through the call chain to model it.
- Verify what the change can reach. A test-only addition runs those tests, not the whole suite.
- Keep wall-clock seconds out of committed comments and docs; write the ratio. The machine that measured them is the fastest one, so the number is wrong everywhere else. A benchmark table names its machine and is the exception.
- Test the language from an e2e fixture: a `.wado` file in `wado-compiler/tests/fixtures/`, expectations in its `__DATA__` section. Nearly everything the language does is stated there, diagnostics included. A fixture states a rejection two ways. `{"compile_error": "…"}` matches the whole report, so writing `":4:13: parse error: …"` pins the position as well as the message. `{"compile_error_codes": ["INVALID_SYNTAX"]}` names the `Code` it was raised under. Kiln is the exception: a generator runs against the filesystem, which a fixture cannot set up.
- Test a stdlib function from the `*_test.wado` beside its module under `wado-compiler/lib/`, not from a new e2e fixture. A fixture is for what the compiler does.
- A regression fixture's shape is what reproduces its bug. When a harness (EMI, the goldens) cannot classify one, fix the harness, never the fixture.
- Where Wado reads a binary input, it reads a text form of it too (`.wat` beside `.wasm`, `.onnxtext` beside `.onnx`), so the repository commits something with a readable diff. A new format joins the same pipeline, and the binary is canonical where the two disagree.
- Write an integration test only for what no fixture can state — the CLI, the loader, `dump` output, a host API. Put it in `tests/integration/` and declare it in that directory's `main.rs`. A file dropped directly in `tests/` becomes its own target, and each one statically links the compiler and wasmtime for another ~150 MB.
- Run `/code-review-response` to answer any review finding, whoever the reviewer is and however it reaches you. A finding arriving as a pull request event is one, and handling it straight from the event skips every step the skill ends with.
- Run `/distill` once a piece of work is done, and again after answering review findings. An extra run costs nothing, so run it the moment you wonder whether you should. §"The Cycle" says where it sits.

## The Wado Language

Much of the syntax is Rust-compatible by design, so most Rust knowledge carries
over. It stops short in a few places: a value never needs `.clone()`, `enum`,
`variant` and `flags` split what Rust puts in one `enum`, and imports follow ES
modules while tuples follow TypeScript.

@docs/cheatsheet.md is the quick reference. For the detailed specification read
`docs/spec-*.md` (one file per area, indexed in `docs/README.md`), or the WEP
that proposed a feature at `docs/wep-*.md`.

`wado doc core:prelude` states every stdlib signature. Read it rather than
guessing where a `&` goes, which differs per method, or hand-rolling a walk over
`chars()` that `split_once`, `find` or `strip_prefix` already does.

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
- `wado-bundled-icu/` - ICU binding for Wado (under development; not bundled yet)
- `docs/` — the language spec (`docs/spec-*.md`), the compiler and formatter guides (`docs/compiler.md`, `docs/optimizer.md`, `docs/formatter.md`), stdlib docs, and the Wado Evolution Proposals (`docs/wep-*.md`).
- `benchmark/`, `wasm-size/` — performance and code-size measurement.
- `cloudflare-worker/` — serves a `wasi:http/service` component from a Cloudflare
  Worker, via jco.
- `package-gale/` — A parser generator compatible with ANTLR4 (`.g4`) in Wado.
- `package-gale-highlight-wado` - A complete `Wado.g4` and a syntax highlighter for Wado source code, built with `package-gale`.
- `package-grog` - A Protocol Buffers compiler in Wado: a `.proto` becomes Wado declarations, and the runtime library encodes them.
- `package-jade` - A JSON Schema 2020-12 validator in Wado.
- `package-marl` - A CommonMark subset in Wado.
- `package-loam` - A tensor compiler in Wado: an ONNX graph becomes Wado source, shapes checked at build time.
- `package-wadopoet` - Builders for generated Wado source, and the reserved vocabulary (generated from `wado syntax --format json`) a minted name must avoid.
- `package-web/` - `wado-lang:web`: the web platform bindings, their browser glue, and `SurfaceDom`, a DOM without a browser engine that serves them under `wado test`, `wado run` and `wado serve`.
- `package-cm-catalog/` - A catalog of Wasm Component Model modules for demo and testing purposes.
- `vendor/` — reference specs and runtimes, as git submodules.

## The CLI

The `wado` binary is implemented in `wado-cli/`. Below, `wado` is shorthand for `cargo run --bin wado --`. `wado --help` lists every subcommand and `wado <command> --help` its flags; the `wado-cli` skill covers the workflows.

The ones you reach for while developing the toolchain:

- `compile` — compile one source file to Wasm or WAT. `-O0` (none) … `-O3` (aggressive), `-Os` (`-O2` + strip symbols); default `-O2`.
- `check` — verify a source file (and its Kiln generators) without emitting Wasm.
- `run` — compile and run a CLI program with wasmtime.
- `test` — run the `test` blocks in Wado source files.
- `serve` — compile and serve an HTTP service.
- `dump` — dump compiler internal state at every stage: AST, modules, symbols, types, TIR, NIR, WIR.
- `query` — ask the language service for hover / definition / references / diagnostics, by position or by `MODULE#SYMBOL` notation. `query inlay-hints` splices the hints into the source, so a misplaced anchor is visible rather than a number to check by hand.
- `format` — format Wado source code. Its rules are in `docs/formatter.md`.

The rest (`init`, `update`, `fetch`, `build`, `publish`, `doc`, `wit`, `syntax`, `lsp`, `clean`) serve packaging, registry, and editor integration.

Behavior that no `--help` will remind you of:

- A program targets a Wasm _world_: `wasi:cli/command` (default), `wasi:http/service`, or the synthetic `test` world. `--world test` exports the entry module's `test` blocks and drops everything else; `serve` and `test` pick their world automatically.
- The world selects the allocator: `bump` for CLI (never frees), `freelist` for HTTP (long-running), `debug` for the test world (never reuses freed memory, poisons it with `0xFF`). E2E tests rely on the test world picking `debug`.
- `wado run` reaches only the directories granted to it: the current one, or exactly the `--dir` grants once any is given. Paths open relative to a grant, so an absolute path never opens.
- The Wado formatter skips `wado-compiler/tests/**` (`[format] exclude` in its `wado.toml`), so an e2e fixture keeps its hand-authored layout. A directory argument is walked from the package enclosing it, so naming a subdirectory honours the exclusion. Naming a file bypasses it, so never `wado format -w` a fixture file directly. When the syntax changes, add tests to `wado-compiler/tests/format.rs`.

## Dependencies

The wasm-tools crates (`wasmparser`, `wasm-encoder`, `wasmprinter`, `wit-parser`, `wat`) are pinned in `[workspace.dependencies]` to the same generation wasmtime depends on, so cargo dedupes them instead of compiling parallel 0.x trees. `mise run check-deps` enforces this (also a CI job) and lists the irreducible exceptions.

When bumping wasmtime, re-align them:

1. Find wasmtime's generation, e.g. `cargo tree -i wasmparser@<ver>`.
2. Re-pin the wasm-tools crates in `Cargo.toml` to that generation (`wat = "~1.<gen>"`, the rest `"0.<gen>"`).
3. `cargo update`, then `mise run check-deps`.

## References

### Wasm and WASI

Wado targets the following Wasm features:

- Wasm 3.0 (released on 2025-09-17), including GC and JSPI
- Wasm Component Model (CM)
  - Design: `vendor/component-model/design/mvp/`
  - Canonical ABI: `vendor/component-model/design/mvp/CanonicalABI.md`
  - Concurrency (async, streams, futures): `vendor/component-model/design/mvp/Concurrency.md`
- WASI 0.3 (or p3, released on 2026-06-11)
  - Fully supported by wasmtime.
  - See wasmtime's P3 support: `find vendor/wasmtime/crates/wasi/src/p3/wit -name '*.wit'`

### Vendor Submodules

`vendor/` contains reference repositories: the specifications for Wasm and the Component Model, plus runtimes such as wasmtime.

To initialize:

```sh
git submodule update --init --recommend-shallow
```

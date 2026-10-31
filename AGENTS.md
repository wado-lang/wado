# The Wado Programming Language

This document describes how to develop the Wado compiler toolchain.

## Principles

- Succinctly — say and write the least that fully conveys the point.
- Fix-forward — fix the cause of a defect and move forward; never backtrack.
- Fix the class — a defect is one instance; fix what admits the class.
- Design is the user's call — a rule invented to soften a fix's side effect is a design decision: propose it with the trade-off.

## Development

This project uses [mise](https://mise.jdx.dev/) to manage dev tools. Project tasks are defined in `mise.toml`. Run `mise tasks` to discover available tasks.

### When Starting a Task

Run the following to set up your development environment:

```sh
mise trust                 # trust the mise.toml config (first time only)
mise run on-task-started   # install project tools
```

Once you cut the branch, attach `wado-lang/wado` to the session with
`add_repo` and `push` access. A session without it cannot subscribe to its
pull request's activity.

### The Cycle

Write the change, commit it, invoke the `/distill` skill, then test. `/distill`
is the last of the editing rather than a phase after it, so one test run at the
end answers for the change and for what `/distill` edited. A run on either side
of it is the same run spent twice.

Each commit runs the tests of what it changed:

| The change touches                  | Run                     |
| ----------------------------------- | ----------------------- |
| The compiler (`wado-compiler/src/`) | `mise run test-e2e`     |
| The stdlib (`wado-compiler/lib/`)   | `mise run test-stdlib`  |
| A package (`package-*/`)            | `wado test <package>`   |
| Any other Rust crate                | `cargo test -p <crate>` |

`wado test` skips a file that passed while nothing it read has changed, so a
rerun after a Wado edit tests only what the edit reached. A rebuilt `wado`
binary reruns everything.

The full run is `mise run test`, then `mise run test-wado`. It runs once,
before the branch goes up for review, and CI runs it on every push.

Having invoked `/distill` on this branch earlier is not a reason to skip the
next one, and answering review findings calls for another. The scope is the
whole branch every time, and what the commits since then made stale is spread
across everything the branch touched. An extra run costs nothing, so run it the
moment you wonder whether you should.

### Common Development Tasks

```sh
mise run test            # check the spec examples, then test Rust crates
mise run test-e2e        # run the compiler's e2e fixtures
mise run test-wado       # test the Wado packages, except the stdlib and Gale
mise run test-stdlib O3  # test the stdlib at one optimization level (CI runs each)
mise run test-gale-o2    # test Gale
mise run check           # run the corpus checks that finish in seconds
mise run format          # format Rust, Markdown, and Wado files
```

## Tooling

- Make every edit with the editing tools, never a shell rewrite (`sed`, `awk`, a script), however bulk the change.
- Run a long job (`mise run test`, `test-wado`, `update-golden-fixtures`) in the background, one per invocation. Run one heavy job at a time; two starve each other.
- `test`, `test-wado`, `test-stdlib`, `test-gale-o2` and `test-fuel` refuse to start while another of the same is running (`test-e2e` counts as `test`, and `update-baselines` as `test-fuel`), naming the holder's pid — where `flock` exists; `scripts/exclusive.sh` says so on stderr and runs unlocked where it does not. Abandoning a job does not stop it, so restarting after an edit means killing that pid first.
- Redirect a job's output to a file and read the file, so what you did not anticipate is still there. Scratch files go in `scratchpad/`: it is git-ignored, so `git add -A` leaves them out, and per worktree, so concurrent sessions do not overwrite each other's. A fixed path under `/tmp` is shared by every session.
- On an internal pull request, CI's `tidy` job regenerates the generated files, applies clippy and format, and pushes `chore: tidy` onto the branch. Pull before pushing.
- A wasm-tools CLI feature a task or skill needs is a `wado-dev-tools` subcommand, built on the wasm-tools crates the workspace pins. Nothing installs the CLI.
- Never edit a generated file: change its generator or source and regenerate. A generated file carries `-diff` in `.gitattributes`, so `git diff`, `git show` and `git log -p` report it as changed without printing it, and the sources stay readable. Regenerate and commit those; do not read them. `--text` prints one where you do want it. `rg` reads them like any other file; `git grep` calls them binary.
- `git diff <base> -- $(scripts/changed-sources.sh)` narrows further, to the changed paths themselves: a stat line or a rename for a generated or vendored file is gone too.

## General Rules

- Write all documentation and comments in clear, simple English.
  - Comments: write one only for what the code cannot say, and make it say why: why this way, a tradeoff, a constraint, a spec or bug reference. It cites no process document (an `AGENTS.md`, a `TODO.md`, a skill, a dead-ends log): those move, and the pointer dangles. Make the code say what it can: rename and decompose until the comment is redundant, then delete it.
  - Invariants: state them as assertions, not comments. An assert is checked; a comment goes stale.
  - Contract messages: write one only for what the condition cannot say, such as a concept's name or a reason, and phrase it as what must hold (`"slots must be a power of two"`). A message that restates the condition is left out.
  - Markdown: the `markdown` skill holds the rules. Read it before writing or editing any `.md` file.
  - Issue references: an issue or pull request in another repository is written fully qualified, `org/repo#num` (`antlr/antlr4#4911`). A bare `#num` means this repository.
  - Timings: no wall-clock seconds in comments or docs. This machine is the fastest one, so write the ratio.
- Commit and push each self-contained unit without asking, never onto `main`.
- Read the whole diff before committing. A trimmed comment or doc keeps every clause that names a pass, a type, a literal, a condition, or the bug behind the code.
- A defect found mid-task is fixed on the branch in hand, never on a side branch or worktree.
- Delete the local branches `git branch --merged main` lists without asking, except `main` and a branch with no commits of its own yet. Never one `--no-merged` lists.
- A compiler rule that blocks an edit is a suspect, not an obstacle to route around: widening a modifier until the build goes green once hid a public-API regression.
- An input nobody would supply is bounded by one constant where it enters, not modelled through the code.
- Perform red/green TDD.
- A compiler bug is always P0 — no exceptions. The instant you suspect one, stop all other work, and as the top priority write a minimal reproducible e2e fixture and fix it.
- A pre-existing issue must be fixed, with TDD when practical.
- Use plain `cargo build` / `cargo run` / `cargo test` (the `dev` profile) for iteration. `Cargo.toml` raises `opt-level` on `wado-compiler`, `wado-dev-tools`, and deps so dev-build runtime is close to release for the parts that matter. `--release` is only for distributing binaries.
- Merge origin/main only through the `git-upstream-sync` skill, conflicts or not; a clean merge still ends with its sanity check.
- A failure that shows only in CI is usually not the environment: a pull request's test jobs run on the branch merged with `main`, not on the branch head. Sync first with the `git-upstream-sync` skill and reproduce on the merged tree before suspecting anything else. `tidy` is the exception, checking out the head ref.
- Test the language from an e2e fixture. The fixtures state nearly everything the language does, diagnostics included; `wado-compiler/AGENTS.md` says how to write one.
- Write an integration test only for what no fixture can state — the CLI, the loader, `dump` output, a host API. Put it in `tests/integration/` and declare it in that directory's `main.rs`. Each test target statically links the compiler and wasmtime for another ~150 MB, so a file in `wado-cli/tests/` becomes one, and one in `wado-compiler/tests/` is not compiled at all: that package lists its targets.
- A test of generated source checks what the text means by running it (`core:eval`), and matches the text only for what running cannot show, such as layout. A text match alone passes a wrong escape against an equally wrong expectation.
- A regression fixture's shape is its point: when a harness cannot classify one, fix the harness, not the fixture.
- Run what the change can reach, not more: a test-only addition does not need the full suite.
- An unexplained failure indicts the measurement as often as the change: re-run the exact command by hand before believing it, and never revert a design over one. zsh does not word-split `$VAR`, so a flag list in one variable reaches a command as one argument.
- A CI-only timeout in a test that reads `wasi:clocks`, once it passes on the merged tree, is the runner's slowness, not the branch.
- Run `/code-review-response` to answer any review finding, whoever the reviewer is and however it reaches you. A finding arriving as a pull request event is one, and handling it straight from the event skips every step the skill ends with.
- Another implementation's code is never a source: its license does not reach this repository, and code written after reading it is a derivative. A rule comes from the specification; an implementation may be run as a black-box oracle. A package's `AGENTS.md` lists what it may read.
- Do not commit a binary: its diff is unreadable. Where a text form means the same, commit that instead (`.wat` for `.wasm`, `.onnxtext` for `.onnx`).

## Instruction Files

The root `AGENTS.md` holds what applies everywhere. A directory's `AGENTS.md`
holds only what to keep in mind on every change there. Neither states what is
obvious, deducible from another rule, or common knowledge.

History, design notes, procedures and open work go elsewhere, linked from
`AGENTS.md` with a one-line summary: a package's into its own directory, a
crate's internals into its module docs.

## The Wado Language

Much of the syntax is Rust-compatible by design, so most Rust knowledge carries
over. It stops short in a few places: a value never needs `.clone()`, `enum`,
`variant` and `flags` split what Rust puts in one `enum`, and imports follow ES
modules while tuples follow TypeScript.

@docs/cheatsheet.md is the quick reference. For the detailed specification read
`docs/spec-*.md` (one file per area, indexed in `docs/README.md`), or the WEP
that proposed a feature at `docs/wep-*.md`. `wado doc <module>` (`core:prelude`,
say) states that module's signatures; read it rather than guess.

## The CLI

The `wado` binary is implemented in `wado-cli/`. In these files, `wado` is
shorthand for `cargo run --bin wado --`. The `wado-cli` skill covers every
subcommand; read it before invoking `wado`.

The Wado formatter skips `wado-compiler/tests/**`, so an e2e fixture keeps its
hand-authored layout. Naming a file bypasses the exclusion, so never
`wado format -w` a fixture file directly. When the syntax changes, add tests to
`wado-compiler/tests/format.rs` and the construct to
`wado-compiler/tests/format.fixtures/all_dirty.wado`.

## Repository Map

- `wado-compiler/` — the compiler: frontend, IR pipeline, optimizer, codegen. The Wado standard library (`core:*`, `wasi:*`) lives in `wado-compiler/lib/`. Internals: `docs/compiler.md`, `docs/optimizer.md`.
- `wado-cli/` — the `wado` binary.
- `wado-host/` — the native host pieces `wado-cli`, `wado-dev-tools` and the compiler's tests share.
- `wado-run-webgpu/` — the `wado run-webgpu` subcommand, a Cargo workspace of its own that no command at the repository root builds or lints. A change that reaches every crate, such as a toolchain upgrade, runs `cargo clippy --all-targets` in `wado-run-webgpu/` too.
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
- `package-eddy/` — a code review bot in Wado, on the Anthropic API for now; later, models of several providers review a change by exchanging opinions. Its docs and research live in this repository.
- `package-gale/` — a parser generator compatible with ANTLR4 (`.g4`) in Wado.
- `package-gale-highlight-html/` — a syntax highlighter for HTML that highlights embedded CSS and JavaScript as those languages, built with `package-gale` on the grammars-v4 grammars.
- `package-gale-highlight-wado/` — a complete `Wado.g4` and a syntax highlighter for Wado source code, built with `package-gale`.
- `package-grog/` — a Protocol Buffers compiler in Wado: a `.proto` becomes Wado declarations, and the runtime library encodes them.
- `package-jade/` — a JSON Schema 2020-12 validator in Wado.
- `package-marl/` — a CommonMark subset in Wado.
- `package-loam/` — a tensor compiler in Wado: an ONNX graph becomes Wado source, shapes checked at build time.
- `package-wadopoet/` — builders for generated Wado source, and the reserved vocabulary a minted name must avoid.
- `package-web/` — `wado-lang:web`: the web platform bindings, their browser glue, and `SurfaceDom`, a DOM without a browser engine that serves them under `wado test`, `wado run` and `wado serve`.
- `package-cm-catalog/` — a catalog of Wasm Component Model modules for demo and testing purposes.
- `vendor/` — reference specs and runtimes, as git submodules; the `vendor-submodules` skill locates and syncs them.

## Wasm and WASI

Wado targets Wasm 3.0 (GC and JSPI included), the Component Model, and WASI
0.3 (p3). The Component Model's design is in
`vendor/component-model/design/mvp/`, with the Canonical ABI in
`CanonicalABI.md` and async, streams and futures in `Concurrency.md` there.
wasmtime's P3 WIT is under `vendor/wasmtime/crates/wasi/src/p3/wit/`.

Wado supports WASI at one exact version: the one wasmtime implements, which
`mise run update-stdlib-wasi` generates the stdlib from. A WASI import whose
version differs from the host's gets no workaround on Wado's side. Matching
semver-compatible versions is wasmtime's job, and Wado waits for wasmtime to
implement it.

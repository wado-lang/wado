# wado-compiler

The Wado compiler crate. The standard library under `lib/` has its own
[AGENTS.md](./lib/AGENTS.md).

## Rules

- The compiler is deterministic: one input emits the same bytes on every run,
  whatever the thread count or host. A lapse shows as drift in the WIR golden
  fixtures (`mise run update-golden-fixtures`), and nondeterminism is a P0 bug.
- Nothing in this crate writes to a stream: `println!`, `eprintln!` and `dbg!`
  are denied at the crate root. A user-facing message goes through `Logger` and
  a developer trace through `compiler_trace!`.
- An `assert!` states an invariant the compiler establishes for itself. What a
  source file can violate is a `Diagnostic`: a panic on user input is a crash,
  whatever it asserts.
- A phase error words itself once, in its `Diagnostic`. It carries no `Display`:
  nothing in the crate can print one, and a second wording drifts from the one
  the user reads. `Display` is for an error the CLI prints itself
  (`CompileError`, `WitEmitError`), or one whose text a `Diagnostic` builder
  reads (`LexError`, `LoadError`).
- `src/codegen.rs` emits the `NirPackage` and `WirPackage` as they are; it
  knows nothing of the earlier phases.
- Only `src/name.rs` knows a name format. Mangling and monomorphization go
  through it.
- Every name the compiler mints for itself starts with one `$`
  (`name::INTERNAL_PREFIX`), which no Wado identifier can spell.
- What makes such a name unique is a serial that advances on read
  (`FunctionContext::fresh_serial`), never a local index the site has yet to
  allocate. A step that reads one before recursing hands its own names to
  whatever nests inside it (issue #1987).
- A `TypeId` is a slot, not a type identity: newtype erasure and the boxing
  rewrite both leave many ids resolving to one type. Compare and key by the
  `TypeKey` that `TypeTable::type_key` answers, never by the id.
- A declaration is identified by its `DefId`, never by its name. See
  [WEP: Declaration Identity](../docs/wep-2026-08-12-declaration-identity.md).
- Walk IR through the visitor utilities, and answer a question with one resolver
  over the IR rather than partial walkers, which each miss a different shape.
- Optimize to the limit correctness allows, and nothing short of it. Wrong code
  is never a trade for speed. A conservatism is not caution but a defect:
  measure what it buys, and delete it when that is nothing.
- This crate must compile for `wasm32-unknown-unknown` (checked in CI). Keep
  OS-dependent `std` modules out of production code.
- `cargo check --all-targets` while iterating, or a type the crate changes
  breaks the uncompiled unit tests where nothing looks. `mise run test-e2e`
  covers O2 and leaves the other levels to CI, whose `ignored` lines are that
  split and not a gap. Run one of them with `mise run test-e2e-level <LEVEL>`
  only when asked, and never set `WADO_FULL_TEST` unless asked by name.

## E2E Tests

A fixture is a `.wado` file in `tests/fixtures/`, its expectations in a
trailing `__DATA__` JSON section whose fields are the `serde` structs in
`tests/e2e.rs`. A path in `__DATA__` is relative to the fixture's own
directory. `tests/` is a package of its own, `wado-compiler-tests`; its
`Cargo.toml` says why.

- A rejection is stated two ways. `{"compile_error": "…"}` matches the whole
  report, so writing `":4:13: parse error: …"` pins the position as well as
  the message. `{"compile_error_codes": ["INVALID_SYNTAX"]}` names the `Code`
  it was raised under.
- A fixture may name a Kiln generator by a `./` path, or by a
  `[build-dependencies]` nickname its `build_dependencies` key maps to a
  generator file. The harness runs it, and the clauses in the generator's own
  source, and writes their outputs under `build/kiln/` in the fixture's
  directory. Fixtures share that directory, so a clause naming its own
  `output_dir` is refused.
- Run `touch tests/e2e.rs` after adding or removing a fixture, or after changing
  `WADO_FULL_TEST`. `datatest_mini` resolves fixtures at macro-expansion time and
  an incremental `cargo test` will not re-expand on its own.
- A fixture's `-O2` run rewrites its WIR golden in `tests/generated/fixtures/`,
  and `prune_orphan_wir_goldens` removes the goldens no fixture writes.
  `mise run update-golden-fixtures` regenerates them all without running the
  tests; CI's `tidy` job runs it.
- `wado dump -Ox file.wado` is how you find `wir_expect:Ox` patterns.
- `wado dump --assert-plan file.wado` shows which operands an `assert` captures
  and which of them a short-circuit can skip.
- `builtin::black_box(value)` returns `value` opaquely, keeping a fixture's input
  off the constant-folding path.

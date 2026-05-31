# Handoff — Stage 5 Elaborator Re-architecture (reify parity)

## Original problem statement (verbatim intent)

Continue Stage 5 of the Wado compiler's "Elaborator Re-architecture" (WEP
`docs/wep-2026-05-26-elaborator-rearchitecture.md`). Stage 5 splits the
elaborator body walk into an `annotate` pass (records facts on
`ModuleSemantics`) and a `reify` pass (mechanically emits TIR), gated by
`WADO_REIFY=1`. **Concrete goal: make `WADO_REIFY=1` produce results identical
to production across all e2e fixtures, clearing reify-specific failures one at a
time.**

Standing constraints:
- Develop ONLY on branch `claude/elaborator-refactoring-stage-5-PVIL0`.
- Commit + push completed work. Do NOT open PRs.
- Comments/docs in English. Never push to a different branch.
- Do not put the model identifier in any committed artifact.
- **CRITICAL PROCESS NOTE (user, verbatim): "環境が不安定ですね。並列でツール実行すると詰まるので、直列にしてください。" / "進めてください。ツールは並列実行すると詰まるので一つずつ。"** — The remote shell channel is unstable. Running tools in parallel JAMS it: large parallel batches get cancelled mid-flight, silently discarding Edits, and the channel replays stale/garbled output. **Work STRICTLY SERIALLY: one (at most two) tool call(s) per turn.** Do NOT chain `sleep`; use `run_in_background: true` + an `until` loop to wait. This was the #1 recurring failure all session — heed it.

## Current state of progress

- Session started at **2666 / 2678** fixtures passing under `WADO_REIFY=1`.
- Now at **2676 / 2678** (two `FAILED` lines = `tuple_for_of` at O0 and O2 =
  **1 unique remaining fixture**). Production is 2678/2678; reify is gated so
  production is unaffected. Confirmed by two independent full-suite runs:
  `test result: FAILED. 2676 passed; 2 failed; 4014 ignored` (831s and 521s runs).
- Five fixtures cleared this session: `tuple_zip`, `if_merged`, `for_merged`,
  `loop_nested`, `opt_sroa_variant_return_if_descent`.

### Git state — VERIFY FIRST

The last commit I verified as pushed was **`e6c99fb7`** ("docs: reify parity
2666 to 2676; log landings 49-52"), with `HEAD == origin == e6c99fb7`, clean
tree. However, the final channel read came back **corrupted** (showed
`HEAD=72aa60d7`, `DIRTY=1`, and a bogus `origin/...` "unknown revision" error —
all inconsistent with the verified push). **Do not trust that last read.**

First action for the next session — run these ONE AT A TIME:
```
git -C /home/user/wado rev-parse --short HEAD
git -C /home/user/wado rev-parse --short origin/claude/elaborator-refactoring-stage-5-PVIL0
git -C /home/user/wado status --short
```
Expected: HEAD == remote == `e6c99fb7` (or later), tree clean. If there is an
uncommitted edit to the WEP doc adding a "**The failure is nondeterministic**"
paragraph to the `tuple_for_of` bullet — **discard it** (`git checkout -- docs/...`).
That edit was speculative and is WRONG: the full-suite evidence shows
`tuple_for_of` fails *deterministically* at O0+O2 in every run. If HEAD is
behind `e6c99fb7`, re-check whether the doc landings are present; re-commit/push
only what's missing.

## Commits this session (chronological)

Prior (already on branch at session start):
- `9e4ce7d7` re-resolve UNKNOWN struct field types via loaded_modules in reify
- `8ca2a779` tir: remove unused find_unique_decl_type_by_name helper

This session:
- `13506f00` **transpose concrete tuple `.zip()` inline in reify** — clears `tuple_zip`
- `dc04987d` **keep trailing if/match/labeled-block value-producing in reify**
- `3820ac05` **lower naked `continue` to for-body break label in reify** — clears `for_merged`, `loop_nested`
- `605c5b0c` **use `block_result_type` for let-chain arm types in reify** — clears `if_merged`
- `72aa60d7` docs update + `cargo fmt` reify.rs (fmt whitespace only; the doc Edit in this commit FAILED to apply — superseded by e6c99fb7)
- `e6c99fb7` **docs: reify parity 2666→2676; landings #49–#52** — the authoritative doc update

## Fixes landed (mental models + exact changes)

All fixes are in `wado-compiler/src/elaborator/reify.rs`. The governing
principle: **reify must mirror the production elaborator
(`elaborator/stmt.rs`, `elaborator/expr.rs`, `elaborator/method_call.rs`)
shape-for-shape.** When a fixture fails, diff reify-vs-production output at the
earliest pipeline stage that differs.

### Verification method (use this — it is fast and decisive)
- `wado dump --tir-resolved -O0 F` / `--tir-monomorphized` / `--nir-lowered` /
  `--no-validate --wat-to-stdout` — run with and without `WADO_REIFY=1`, `diff`.
  WAT (`--no-validate --wat-to-stdout`) is the most decisive: it shows the exact
  codegen divergence. NOTE: TIR text dumps print types *by name*, so they hide
  ref-exactness / TypeId-identity differences — WAT does not.
- `wado dump` does NOT support `--world`; it auto-detects. The CLI default world
  WAT can be identical while the test world diverges (this is exactly the
  `tuple_for_of` situation).
- For test-world fixtures, reproduce with `wado test F` (a `.wado` file with a
  `test "..." {}` block and `__DATA__ {"test": {}}`).
- e2e harness: `WADO_REIFY=1 cargo test -p wado-compiler --test e2e -- <name>`.
  O0/O2 run by default; O1/O3/Os need `WADO_FULL_TEST=1`. `touch
  wado-compiler/tests/e2e.rs` after adding/removing fixtures.

### #49 tuple_zip — `reify_method_call` "zip" arm (~reify.rs:7059)
Production (`method_call.rs:367-435`): a concrete tuple-of-tuples `.zip()`
transposes inline (build nested `FieldAccess` → `TupleLiteral` columns); only a
type-pack receiver defers to a `TupleZip` TIR node (monomorphizer expands it).
Reify was emitting `TupleZip` unconditionally — but non-generic bodies never
reach the monomorphizer, so it hit `lower/translate.rs:1135` `unreachable!`.
Fix: `if self.type_contains_pack(base_type_id)` → `TupleZip`, else transpose
inline. Reify has its own `type_contains_pack` (reify.rs:5038),
`as_tuple`/`make_tuple` on the type table.

### #50 trailing value blocks — `reify_block` (reify.rs:1730) + new `reify_if_stmt_with_expected`
Production `resolve_block` (stmt.rs:40-66) special-cases FOUR trailing-statement
forms when `expected_type.is_some()`: `Expr`, `If`, `Match`, `LabeledBlock`.
Reify only handled `Expr`. Added the other three. Also: stmt-position
`reify_if_stmt` (reify.rs:3631) now emits a value `If` *expression* statement
(via `agree_branch_types` for the result type) mirroring `resolve_if_stmt`
(stmt.rs:992) — so a tail `if` produces its value even with no `expected_type`.
This cleared `opt_sroa_variant_return_if_descent` as a by-product (#52).

### #51a continue — `reify_stmt` Continue arm (reify.rs:1812)
Inside a C-style `for`, `continue` must `break` to the synthetic body label so
the loop `update` runs before the next iteration. Reify emitted a bare
`Continue` → infinite loop → epoch-interrupt trap. Mirror `resolve_continue`
(stmt.rs:2906): if `ctx.for_continue_labels.last()` is Some, emit
`Break { label: Some(body_label), value: None }`, else `Continue`.

### #51b let-chain arm types — `reify_let_chain_stmts` (reify.rs:3438)
The then/else arm types were computed by a hand-rolled "if last stmt is
`TirStmtKind::Expr` take its type_id, else UNIT" check. For a block ending in a
value `If`/`Match`/nested chain that returned UNIT, collapsing the two-arm
`Match`'s `match_type` to UNIT, so `agree_branch_types` dropped the branch
values and the join emitted `unreachable` (trap in `classify_opt`). Fix: use
`crate::tir::block_result_type(&inner_block)` exactly as
`resolve_let_chain_stmts` (stmt.rs:1140) does.

## THE ONE REMAINING FAILURE: tuple_for_of (test world only)

Fixture: `wado-compiler/tests/fixtures/tuple_for_of.wado` (test world,
`{"test": {}}`). Fails at O0 and O2.

Symptom: codegen validation panic at `codegen.rs:45`:
`Internal compiler error: WIR pipeline generated invalid core Wasm module` /
`Validation error: type mismatch: expected (ref null $type), found (ref null
$type) (at offset 0x...)` — and at a different offset
`expected (ref $type), found (ref (exact $type))`. **The two types differ only
in GC ref EXACTNESS** (`exact` vs non-exact, and null vs non-null variants).

Key observations (established, trust these):
- The **CLI-world WAT is byte-identical** reify-vs-production (`diff` of
  `--no-validate --wat-to-stdout` = 0 lines). The bug is test-world-specific.
- A **single** tuple for-of test body compiles+runs fine under `wado test`
  (verified: "nested tuple for-of expansion" alone → ok; the 5
  homogeneous/empty/single/mutable/enum-const bodies together → ok).
- It fails when **several tuple for-of bodies coexist** in the test world,
  AND specifically reproduced with the "enumerate with trait dispatch" /
  heterogeneous-dispatch bodies (a 2-body file with enumerate+`.describe()`
  trait dispatch reproduced `expected (ref $type), found (ref (exact $type))`).
- So: the divergence is in **how reify interns / reuses the heterogeneous tuple
  struct type across multiple functions** — the exactness flag on the interned
  `(ref $T)` differs from production. Each body in isolation is fine; the
  cross-function interning order/state is what diverges.

Where to look:
- `reify_tuple_for_of` (reify.rs:3016) — the compile-time unroll. It calls
  `as_tuple`, `make_tuple` (for enumerate's `(i32, elem)` tuple), `reify_pattern`,
  builds `FieldAccess` into per-element `LabeledBlock`s.
- The production analogue is `resolve_tuple_for_of` / `resolve_for_of`
  (stmt.rs:2079). Diff the **monomorphized TIR** is NOT enough (types print by
  name); you must inspect the **interned ResolvedType / TypeId exactness**.
  Grep `exact` across `wado-compiler/src/` — exactness lives in `wir.rs`,
  `tir.rs`, `nir.rs`, `wir_build/types.rs`, `codegen/emit.rs`,
  `optimize/dce.rs` (`struct_exact`/`variant_exact`/`enum_exact` sets).
- Likely culprit: a tuple struct type interned with the wrong exactness, or two
  near-identical tuple types (one exact, one not) created across the unrolled
  per-element blocks of different test functions, then a `local.set` / branch
  join expects one and gets the other. Compare where production sets exactness
  on the tuple struct ref vs where reify does (or fails to).

Reproduction harness (fast):
```
cp wado-compiler/tests/fixtures/tuple_for_of.wado /tmp/tfo.wado
WADO_REIFY=1 ./target/debug/wado test /tmp/tfo.wado   # panics at codegen.rs:45
```
Minimal 2-body repro that triggered `(ref (exact $T))`: a file with a `Describe`
trait (impls for i32/String) + a "basic heterogeneous" body
(`for let v of [42,"wado",true] { results.push(v.describe()) }`) plus an
"enumerate with trait dispatch" body.

## Failed approaches / process lessons to AVOID

1. **DO NOT batch tool calls in parallel.** Every large batch this session got
   cancelled and silently dropped Edits, then replayed garbled/stale output
   (including a corrupted final git read). One call per turn. To wait on a
   long command use `run_in_background: true` + a separate `until [ -f done ];
   do sleep 5; done` waiter — never chain foreground `sleep`s (the harness
   blocks them).
2. **DO NOT trust a TIR text dump to prove parity.** `--tir-monomorphized` was
   byte-identical for `if_merged` while the compiled WASM differed — because
   types print by name and hide ref-exactness/TypeId differences. Use
   `--no-validate --wat-to-stdout` (and, for test-world bugs, `wado test`).
3. An Edit whose `old_string` doesn't match the file (e.g. wrong indentation,
   or a `→` arrow vs `->`) silently fails; the doc commit `72aa60d7` shipped
   only fmt whitespace because its doc Edit didn't match. Always Read the exact
   surrounding text first, and verify the commit's `--stat`.
4. The "tuple_for_of is nondeterministic" hypothesis is **wrong** — discard any
   uncommitted doc edit claiming that. It fails deterministically at O0+O2.

## Immediate next actions

1. Verify/clean git state (commands above); ensure HEAD==remote==`e6c99fb7`,
   tree clean; discard any stray uncommitted doc edit.
2. Attack `tuple_for_of`: diff the interned tuple struct type exactness
   reify-vs-production across the multiple test functions. Start from
   `reify_tuple_for_of` (reify.rs:3016) and the enumerate `make_tuple` path;
   compare against `resolve_tuple_for_of` (stmt.rs:2079). The error is purely
   ref-exactness, so find where production stamps exactness on the tuple ref and
   make reify match.
3. After it's green: byte-for-byte WAT parity check, run the full reify suite
   (`WADO_REIFY=1 cargo test -p wado-compiler --test e2e`) to confirm
   **2678/2678**, update the WEP doc (landing #53 + bump the count to 2678),
   `mise run format`, commit, push.
4. Then Stage 5 reify parity is complete; the WEP's Stage 6/7 checkboxes are the
   next track.

## Key files
- `wado-compiler/src/elaborator/reify.rs` — the reify pass (single ~9000-line file).
- `wado-compiler/src/elaborator/stmt.rs` / `expr.rs` / `method_call.rs` — production elaborator (the parity oracle).
- `wado-compiler/src/elaborator/orchestration.rs:1136` — the `use_reify` gate (WADO_REIFY + non-stdlib + not building snapshot).
- `wado-compiler/src/lower/translate.rs:1135` — `TupleZip` unreachable! (the #49 trap site).
- `docs/wep-2026-05-26-elaborator-rearchitecture.md` — WEP; landing log + status (line ~428 count, ~959 landings, ~998 remaining-clusters).
- `wado-compiler/tests/fixtures/tuple_for_of.wado` — the remaining failing fixture.

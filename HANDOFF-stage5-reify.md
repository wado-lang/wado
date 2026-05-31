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

---

## ⚠️ AUTHORITATIVE CORRECTION (2026-05-31 session 2 end) — supersedes all earlier tuple_for_of notes above

Earlier byte-forensics paragraphs in this file (claims of "both 13834 bytes / 34
differing bytes", "WAT byte-identical", "WIR identical", "codegen type-index
ordering", "nondeterministic") were produced from a **corrupted shell channel
AND the wrong CLI flag** (`-w` is invalid; the correct flag is `--world test`).
They are WRONG. Ignore them. The facts below were re-established with the
correct flag and verified.

### Correct reproduction
```
cp wado-compiler/tests/fixtures/tuple_for_of.wado /tmp/tfo.wado
./target/debug/wado          compile --world test --no-validate -O0 -o /tmp/p.wasm /tmp/tfo.wado
WADO_REIFY=1 ./target/debug/wado compile --world test --no-validate -O0 -o /tmp/r.wasm /tmp/tfo.wado
./target/debug/wado          compile --world test --no-validate -O0 --wat-to-stdout /tmp/tfo.wado > /tmp/p.wat
WADO_REIFY=1 ./target/debug/wado compile --world test --no-validate -O0 --wat-to-stdout /tmp/tfo.wado > /tmp/r.wat
```
(`--world test` IS accepted by `compile`/`dump`; `dump` even auto-detects, but
pass it explicitly. `wado test /tmp/tfo.wado` reproduces the validation panic.)

### Verified facts
- WAT sizes: prod 9149 lines, reify 9107 (−42). `.wasm`: prod 33268 B, reify
  33128 B. These are LARGE structural differences, not subtle renumbering.
- **Function-signature type pool differs by exactly one, order-independently.**
  Compare with:
  ```
  grep -E "^\s+\(type \(;" /tmp/p.wat | sed -E 's/;[0-9]+;//' | sort > /tmp/pts.txt
  grep -E "^\s+\(type \(;" /tmp/r.wat | sed -E 's/;[0-9]+;//' | sort > /tmp/rts.txt
  comm -23 /tmp/pts.txt /tmp/rts.txt   # in prod, not reify
  comm -13 /tmp/pts.txt /tmp/rts.txt   # in reify, not prod
  ```
  Result: prod has **178** signatures, reify **177**. The single signature
  present in prod and ABSENT from reify is:
  `(func (param (ref 2)) (result (ref 5)))`.
  Nothing is present in reify but absent from prod. So reify is *missing* a
  function whose signature is `(ref 2) -> (ref 5)` (a function taking struct
  type #2 and returning struct type #5).
- Consequently every func-type index ≥108 is shifted by one in reify, and the
  data-segment (string-pool) order also differs (prod data[0]="int(" vs reify
  data[0]="str(" — reify walks the `impl Describe` methods / string literals in
  a different order), with one fewer data segment (88 vs 87). The index shift is
  what surfaces as the validator error `expected (ref null $type), found (ref
  null $type)` (same printed name, mismatched index).
- Accumulation-dependent (re-confirmed): bodies 1–9 pass; the full 10-body
  fixture fails; bodies 1–9 + a *different* 10th body pass; {traits, bodies 1–5,
  body 10} pass; {traits, bodies 6–9, body 10} pass. The trigger needs bodies
  from BOTH groups plus body 10 — i.e. enough accumulated functions that one
  specific helper/instantiation reify fails to emit.

### Correct diagnosis
This is a **real semantic divergence in what TIR/functions reify generates**,
not codegen nondeterminism or ref-exactness. With the full set of test bodies,
reify fails to emit one function (signature `(ref 2) -> (ref 5)`) that
production does. The most likely candidate is a **monomorphized method
instantiation or a `$value_copy`/iterator helper** for one specific
heterogeneous-tuple element type that reify either dedups away or never queues
for instantiation, because reify's cross-function instantiation collection walks
the bodies in a different order/shape than production's. (`(ref 2) -> (ref 5)`:
takes struct #2, returns struct #5 — identify these two structs from the WAT rec
group; #5 is very heavily used, likely `String` or the Array/box type.)

### Designed next steps (need a STABLE channel — this session's was unusable for the loop)
1. Identify the missing function: in production's WAT, find the `(func (;N;))`
   whose declared type is the `(ref 2) -> (ref 5)` signature, and read its body
   to learn what it is (name via the `name` custom section if present, else infer
   from its body — `struct.new`, the method it calls). Identify structs #2 and
   #5 from the leading `(rec ...)` type group.
2. Find where production queues that function for codegen and reify does not.
   Prime suspect: `monomorphize/func_inst.rs`
   `collect_func_instantiation_sites` / the value-copy or iterator-helper
   synthesis — whatever generates per-element-type helpers for tuple for-of.
   Compare the set of instantiations collected under reify vs production for the
   full fixture (instrument the collection to log each queued
   `(name, signature)` and diff).
3. The likely fix is in reify's TIR for `reify_tuple_for_of` (reify.rs:3016) or
   in how reify drives instantiation collection — ensuring the same helper for
   that one element type gets generated. It is feature-level, not a one-line
   shape replay.
4. Verify: `cmp /tmp/p.wasm /tmp/r.wasm` identical → `WADO_REIFY=1 cargo test -p
   wado-compiler --test e2e -- tuple_for_of` green → full suite **2678/2678**.

### Process note
The remote shell channel in this session repeatedly cancelled multi-call turns
and replayed stale/garbled output, which produced the wrong earlier diagnosis.
Do the byte/WAT forensics loop ONE command per turn, write results to files,
and re-verify any surprising number before building a theory on it. Confirm the
exact CLI flags from `--help` first (the `-w` vs `--world test` mistake cost a
whole investigation pass).

### FURTHER REFINEMENT (same session, deeper) — the divergence is in the FUNC-TYPE INTERNING POOL

After re-running with the correct `--world test` flag, narrowed further:

- **Named function symbols are IDENTICAL** between prod and reify: extract with
  `grep -oE "\\\$[a-zA-Z0-9_:/.^]+" /tmp/X.wat | sort -u` → 86 symbols each,
  `comm` shows zero difference. So reify is NOT missing or adding any *function*.
  The function COUNT is also identical (11 core funcs).
- The one signature present in prod and absent in reify —
  `(func (param (ref 2)) (result (ref 5)))` (= `(boxed-i32) -> String`; struct
  #2 = `(struct (field (mut i32)))`, struct #5 = `(struct (field (mut (ref 3)))
  (field (mut i32)))` = the String struct, `(ref 3)` = `(array (mut i8))`) — is
  declared at type index 108 in prod but **referenced nowhere** (`grep -c "type
  108" /tmp/p.wat` = 1, the definition line only). It is an *unused* interned
  function signature.
- So: production's function-type interning pool contains one extra (unused)
  signature that reify's does not. This shifts every subsequent func-type index
  by one between the two builds and co-occurs with a different data-segment
  (string-pool) ordering (prod data[0]="int(", reify data[0]="str(").

Why this makes reify INVALID (working theory): the validator error
`expected (ref null $type), found (ref null $type)` is an internal
inconsistency *within reify's own module* — two sites compute a struct/func
type index via different routes and, because reify's interning order differs,
they disagree. The extra unused type-108 in prod is a *symptom* of the ordering
difference, not the cause; the cause is that reify interns the function
signatures / referenced GC types in a different ORDER than production (driven by
walking the impl methods / per-element-type helpers in a different order across
the accumulated test bodies), and somewhere a type index is captured before vs
after a particular signature is interned.

This is squarely **codegen/monomorphization type-interning order**, confirmed
not to be a missing-function or wrong-TIR-shape bug. It is the hardest class
(needs the instrument→rebuild→diff loop on a stable channel). Concrete next
move: instrument the function-type interner (where `(func (param..) (result..))`
signatures are added to the module's type section — likely in `codegen/emit.rs`
or `wir_build/` / the Package type registry) to log each signature in insertion
order for both builds, diff to find the FIRST divergent insertion, and trace
which codegen walk produced it in a different order under reify. Then make that
walk order-independent (canonical sort) so codegen no longer depends on the
front-end's interning order — consistent with the WEP `codegen.rs` principle.

### Honest status
tuple_for_of is THOROUGHLY characterized but NOT fixed. The remaining work is a
codegen type-interning-order fix that needs sustained byte/WAT forensics on a
stable shell channel (this session's channel cancelled multi-call turns and
replayed garbled output, which already caused one wrong diagnosis — corrected
above). All findings here are re-verified with the correct `--world test` flag.

## ✅ ROOT CAUSE FOUND (verified from WAT, consistent) — tuple_for_of

In the "basic heterogeneous" body `for let v of [42,"wado",true] { results.push(v.describe()) }`:
- **Production** emits three distinct calls: `i32^Describe::describe`,
  `String^Describe::describe`, `bool^Describe::describe` (p.wat lines 2051/2062/2073).
- **Reify** emits `bool^Describe::describe` for ALL THREE elements
  (r.wat lines 2050/2061/2072). The i32 and String receivers are passed to
  bool's method → invalid module; and `i32^Describe::describe` is never called,
  so it's never emitted (the missing `(ref 2)->(ref 5)` = `(boxed i32)->String`
  signature / the only symbol unique to prod = `$/tmp/tfo.wado/i32^Describe::describe`).

Mechanism: the tuple for-of body is compile-time-unrolled once per element. The
body has a SINGLE `v.describe()` source node = one `AstId`. But dispatch is
recorded in `method_dispatch: IndexMap<AstId, MethodDispatch>`
(sem/types.rs:159) — one entry per AstId. Annotate runs
`resolve_tuple_for_of` (stmt.rs:2333) which re-resolves the body N times
(stmt.rs:2501 `resolve_block(&for_of.body, …)` inside the per-element loop);
each resolution overwrites `method_dispatch[mc.id]`, so only the LAST element's
dispatch (bool) survives in the map. Production never reads the map back — it
builds TIR inline as it resolves each element — so it's correct. Reify
(`reify_tuple_for_of`, reify.rs:3016) unrolls N times calling `reify_block`
per element, and every `reify_method_call` reads the single surviving
`method_dispatch[mc.id]` (bool). Hence all-bool.

This is the SAME class of "one source AstId elaborated in N type contexts"
problem the assert-slot / local-frame walk-order invariants (Gap 5/7) solve
with a per-FunctionContext counter. tuple_for_of dispatch needs the analogous
treatment.

### Fix options (pick one)
1. **Per-unroll dispatch list (preferred, mirrors Gap 7).** Change annotate to
   record, for nodes inside a tuple-for-of body, a *sequence* of dispatches in
   unroll order, and have reify consume them in lockstep via an unroll counter
   on `FunctionContext` (like `next_assert_id`). Cleanest conceptually but
   touches the map shape and every per-element-varying annotation (not just
   method_dispatch — also expression_types, operator_dispatch, coercions, etc.,
   all of which are AstId-keyed and would be overwritten the same way for ANY
   body whose per-element types differ).
2. **Reify re-derives dispatch from the receiver type.** In
   `reify_tuple_for_of`, for each element, the binding local `v` has the correct
   concrete `elem_type`. If reify could re-resolve `v.describe()` against
   `elem_type` (method lookup by receiver base type + trait + method name) it
   would get the right FunctionRef. But reify is meant to be mechanical (no
   method lookup) — this re-introduces dispatch logic into reify.
3. **Annotate resolves the tuple-for-of body ONCE per element but reify also
   resolves once per element AND annotate stores per-element facts under
   synthetic AstIds.** Too invasive.

NOTE option 1's scope concern is real: it's not only `describe()`. ANY
AstId-keyed fact recorded while walking the unrolled body differs per element
when the element types differ (expression_types of `v`, the `.push()` receiver
generic args, string-template format dispatch, etc.). The all-bool symptom is
just the most visible. A correct fix must make reify see the right *per-element*
facts for the whole body, not just method_dispatch. That strongly favors a
structural approach: have annotate record the tuple-for-of body's facts keyed by
(AstId, element_index), or have reify re-run annotate's body walk per element
with the element type bound (i.e. reify the body the same N-context way
production resolves it). The latter — reify drives a per-element re-annotation
of the body — is likely the real fix and matches production's structure most
closely.

### Key code refs for the fix
- `method_dispatch` map: `sem/types.rs:159` (IndexMap<AstId, MethodDispatch>).
- record: `elaborator.rs:531 record_method_dispatch` → `.insert` at :542.
- production unroll (the oracle): `stmt.rs:2333 resolve_tuple_for_of`, body
  re-resolved per element at `stmt.rs:2501`.
- reify unroll: `reify.rs:3016 reify_tuple_for_of`, body re-reified per element
  at `reify.rs:3152 reify_block(&for_of.body, …)`.
- reify dispatch consumer: `reify.rs:7246` (reads method_dispatch[method_call.id]).
- precedent for per-instance counter: `FunctionContext.next_assert_id` (Gap 7).

## CORRECT FIX DESIGN — tuple_for_of (ready to implement; needs a stable channel)

Root cause (proven from WAT): a compile-time-unrolled tuple `for-of` elaborates
ONE source body (fixed AstIds) in N type contexts. All AstId-keyed annotation
maps are overwritten, so only the LAST element's facts survive; reify reads
those → every unrolled element dispatches to the last element's methods.

There are ~10 AstId-keyed annotation maps in `sem/types.rs` that can vary
per element and that reify reads (≈45 read sites in reify.rs):
`method_dispatch` (≈9 reads), `expression_types` (≈8), `generic_instantiations`
(≈2), `coercions`, `key_value_coercions` (≈3), `desugars`, `operator_dispatch`,
`static_method_dispatch`, `closure_captures`, `for_of_iterator` (≈1). A correct
fix must give reify the PER-ELEMENT value of ALL of them inside the body — not
just method_dispatch (e.g. `{w}` template formats i32/String/bool via different
Display dispatch; `v`'s expression_type differs per element; a generic call on
`v` instantiates differently).

### Chosen design: per-element annotation OVERLAY (mirrors Gap 5/7 per-instance counter)

Annotate captures, per tuple-for-of element, the body's annotation entries;
reify shadows its lookups with element i's overlay while reifying element i.
This keeps reify building the TIR (so all of reify's own counters — locals,
labels, assert ids — stay self-consistent, unlike splicing annotate's TIR) and
only borrows the per-element FACTS. This is the architecturally correct fit for
the WEP (annotate provides facts; reify emits TIR).

Capture (annotate, `resolve_tuple_for_of` stmt.rs:2333):
- The body's AST nodes are walked for the first time here, so before the element
  loop their AstIds are absent from every map. IndexMap preserves insertion
  order and overwrites update in place (position stable). So: snapshot each
  map's `.len()` as `len_before` right before the element loop. After resolving
  element i's body, the body's entries are exactly `map.iter().skip(len_before)`
  (same key set every element — overwritten in place, so the tail slice is
  stable). Clone that tail slice for each of the 10 maps into an
  `ElementOverlay { method_dispatch: IndexMap<AstId,_>, expression_types: …, … }`.
- Push overlays into a new field
  `sem.types.tuple_for_of_overlays: IndexMap<AstId /*for_of.id*/, Vec<ElementOverlay>>`.
- Implement capture as ONE helper `ElementOverlay::capture(&TypeAnnotations,
  len_before_per_map)` that enumerates all 10 maps in a single place
  (maintainable — new maps get added here once).

Consume (reify, `reify_tuple_for_of` reify.rs:3016):
- Read `self.sem.types.tuple_for_of_overlays.get(&for_of.id)` → `&Vec<ElementOverlay>`.
- Reify holds a new field `tuple_overlay_stack: Vec<ElementOverlay>` (a STACK to
  support nested tuple-for-ofs). Before `reify_block(&for_of.body…)` for element
  i, push `overlays[i].clone()`; after, pop.
- Route reify's reads of the 10 maps through accessor methods that check the
  top-of-stack overlay first, then fall back to `self.sem.types.<map>`:
  e.g. `fn ann_method_dispatch(&self, id: AstId) -> Option<MethodDispatch>`.
  Replace the ≈45 `self.sem.types.<map>.get(&id)` sites with the accessors.
  (Body AstIds hit the overlay; everything else falls through to sem unchanged.)

Why other designs were rejected:
- Splicing annotate's per-element TIR blocks (M3): violates the WEP "reify
  rebuilds TIR" principle and couples annotate/reify local+label+assert counters
  for the body — fragile.
- Reify re-deriving dispatch by method lookup (M2): re-introduces method-lookup
  logic into reify, explicitly rejected by Stage 5 (Gap 2).
- Cloning the body AST with fresh per-element AstIds: requires a deterministic
  fresh-AstId scheme both passes agree on; AstIds are bind-phase-dense and
  inventing reproducible ones mid-elaboration is fragile.

### Implementation checklist (do on a STABLE channel, build+test after each step)
1. `sem/types.rs`: add `pub(crate) struct ElementOverlay { …10 IndexMap fields… }`
   with `capture(types, &lens)->Self`; add field
   `tuple_for_of_overlays: IndexMap<AstId, Vec<ElementOverlay>>` to TypeAnnotations
   (+ Default). Also thread it through `semantics.rs` merge (the per-module →
   global SymbolKey remap at semantics.rs:994+ — decide whether overlays need
   remapping or stay per-module; tuple-for-of is always within one function/module
   so per-module AstId keying is fine, but verify the merge doesn't drop the new map).
2. `stmt.rs resolve_tuple_for_of`: snapshot the 10 lens before the loop; after
   each element capture+push; store the Vec under for_of.id.
3. `reify.rs`: add `tuple_overlay_stack: Vec<ElementOverlay>` to Reify::new;
   add the 10 `ann_*` accessor methods; replace the ≈45 read sites; in
   `reify_tuple_for_of` push/pop the element overlay around `reify_block`.
4. Verify: `cmp <(wado compile --world test --no-validate -O0 --wat-to-stdout F)`
   reify-vs-prod byte-identical; then `WADO_REIFY=1 cargo test -p wado-compiler
   --test e2e -- tuple_for_of` green; then full suite → 2678/2678; update WEP
   landing #53 + count; `mise run format`; commit; push.

### Verified facts to anchor the work
- prod WAT: `i32^/String^/bool^Describe::describe` for the 3 elements;
  reify WAT: `bool^Describe::describe` for all 3 (the bug, lines ~2050/2061/2072).
- Only symbol unique to prod: `$<file>/i32^Describe::describe` (never emitted by
  reify because never called). Missing func-type `(ref 2)->(ref 5)` =
  `(boxed-i32)->String` is that function's signature.
- Correct repro flag is `--world test` (NOT `-w`). Use `wado test F` to see the
  validation panic; compare raw `.wasm` or `--wat-to-stdout` (both with
  `--world test`).

### Process note (this session)
The remote channel degraded to corrupting/injecting text into stdout, base64,
AND file reads, which makes the 45-site edit→build→byte-verify loop unsafe. The
diagnosis and design above are complete and verified; implementation deferred to
a stable channel so the result meets the "only correct code on a correct design"
bar rather than a blind large refactor.

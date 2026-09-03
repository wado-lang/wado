# Wado Optimizer

The optimizer rewrites the Normalized IR (NIR; see [WEP: NIR Layer](./wep-2026-05-11-nir.md)) in place before lowering to WIR, then runs a smaller set of WIR-level passes before Wasm emission. Pass span names used by `WADO_LIST_PASSES` / `WADO_SKIP_PASS` / `WADO_DUMP_PASS_*` carry a `nir/` or `wir/` prefix.

This document is the inventory, one line per pass. The canonical pass order is the code: the `run_pass` sequence in `src/optimize.rs` and the phase sequence in `src/wir_optimize.rs`, each position justified in the comment beside it. Per-pass design lives in that pass's module doc.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a complex compiler transformation — it keeps the compiler small, leverages the runtime JIT, and produces smaller output (`select` for branchless conditionals, `array.copy`/`array.fill` for bulk ops, `br_table` for dense matches).

## Optimization levels

All levels run DCE on functions, types, and globals.

| Flag            | Iterations | Inline budget | Notes                                           |
| --------------- | ---------- | ------------- | ----------------------------------------------- |
| `-O0`           | 0          | N/A           | DCE only + `match_to_switch` + backend rewrites |
| `-O1`           | 2          | 4             |                                                 |
| `-O2` (default) | 15         | 16            |                                                 |
| `-O3`           | 20         | 26            |                                                 |
| `-Os`           | 15         | 16            | strips the Wasm name section                    |

The inline budget counts emitted Wasm instructions on the callee's hot path, not NIR nodes — see [`inline`](#nir-passes) for the weights. `--optimize-inline-growth <pct>` additionally bounds how far inlining may grow the whole unit; no level sets it by default.

The fixed-point loop exits early on convergence, so a pass must report a change only when it made one, never when it merely found work to look at. A `gate_only!` pass reports to the dirty-set gate alone and never extends the loop.

A run that reaches the cap logs it at debug level, naming the passes still reporting changes. At `-O2`/`-Os` and `-O3` that is also a `debug_assert`: their caps are sized so the loop converges under them. `-O1`'s smaller number of rounds and an explicit `--optimize-iterations` are budgets, and say nothing about convergence.

The backend-required rewrites (`select_lowering`, `multi_value_return`, `freeze_pure_arith`) and `match_to_switch` run at every level, including `-O0`.

## Architecture

The optimizer runs on a two-tier NIR: a skeleton arena carrying effect order, control flow, and allocation, plus a hash-consed graph of pure values the skeleton reaches through promoted operands. Local rewrites are rules on a worklist engine over one function at a time, scheduled by a per-function dirty-set gate; promoted values are extracted back to concrete form once, at WIR build. Whole optimizations fall out of that structure rather than existing as passes — CSE and GVN out of hash-consing, pure copy propagation out of shared value identity.

The design, its soundness invariants, the standing "do not reintroduce" rules, and the open architectural work are [WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md). This document does not restate them.

## Pipeline

`optimize.rs` orchestrates the NIR stages; `wir_optimize.rs` runs the WIR stages.

1. Early DCE — remove unreachable functions/types/globals.
2. Before the loop: cold-region outlining, dense `Match` → `Switch` over global initializer bodies, then the early arithmetic promotion.
3. Fixed-point loop (skipped at `-O0`): container SROA, peephole (pre-inline), value-copy demotion, parameter SROA, variant-return scalarization, inlining, peephole (post-inline), let-block flattening, SROA, copy propagation, dead-argument and dead-return elimination, constant folding, parameter specialization, LICM, template hoisting.
4. Post-loop, once: field scalarization, store-load forwarding, template-wrapper cleanup, constant-object globalization, a final folding pass, scalar-temp forwarding, and clone forwarding.
5. Final DCE.
6. Field promotion and the bounds-check work it unblocks (`promote_fields`, then the `condition_implication` rerun and `loop_version_bce`); skipped at `-O0`.
7. Backend-required rewrites (all levels): select lowering, multi-value returns, and the final arithmetic freeze.
8. WIR-level passes — see [WIR optimizations](#wir-optimizations).

## NIR passes

Allocation and aggregate:

- `inline` — replace calls to small, non-recursive functions with their body; reference parameters and receivers inline too. `#[inline]` raises the budget 5x, `#[inline(always)]` forces it, `#[inline(never)]` and cold call sites opt out. A callee over budget as written is re-read under the constants its callers pass. `--optimize-inline-growth` additionally caps what the pass adds to the whole unit; no level sets it.
- `cold_outline` — move what a `cold_path()` marker opens into a function of its own, so `inline`'s cold discount describes the callee. The caller keeps the branch; the marked arm becomes a call. A region moves when control cannot leave it and every local it touches is one the call can hand over. Runs once, before the loop, so the inliner never sees the unsplit shape. It costs `sieve` 4.5% for no reason the IR shows, and a marker in the middle of a loop body is one it cannot take. A function's root block is deliberately not a region — see the pass's module doc.
- `sroa` — decompose non-escaping struct/tuple locals into scalar locals. The highest-impact WasmGC pass.
- `container_sroa` — turn `List<Struct>` / `List<Tuple>` into parallel per-field lists (array-of-structs → struct-of-arrays).
- `sroa_param` — replace a struct reference parameter with the one field the callee reads, unwrapping the box that `&T` values allocate. A multi-field struct is scalarized on a clone, so the callers that pass a whole struct keep the original; how the callee holds the field decides whether the scalar arrives by value or by reference, and a call site that would have to read the field ahead of an effectful later argument is refused.
- `sroa_variant_return` — rewrite a variant return into a `[tag, slots…]` tuple, so a `Result`-returning call stops being one opaque boxed value to every later pass. The return-position dual of `sroa_param`; `multi_value_return` then flattens the tuple to the Wasm multi-value ABI. See [WEP: Variant Return Scalarization at NIR](./wep-2026-08-03-variant-return-abi.md).
- `elide_box_local` — collapse a box bound once and read once into its inner value.
- `drop_value` — a value in discarded position keeps only its effects: a value-producing `ExprKind::LabeledBlock` in statement position becomes the value-discarding `StmtKind::LabeledBlock`, and every `break L: v` targeting it gives up its operand, decomposed into the statements its own operands' effects need. `let _ = xs.pop()` is the shape it is for. `elide_local` demotes the dead binding to `Expr(block)` and stops there, because the element read inside the `Option` may trap and so the aggregate around it is not deletable whole. A statement counts as discarded only when something follows it in its block, or when that block is the function root: WIR decides "value region" by a block expression's own type, so a `match` arm whose result is dropped still expects its last statement to leave a value. Not extended to a discarded `Expr(aggregate)` statement, which does not converge against `sroa_variant_return`; the pass's module doc says why.
- `string_push` — expand a short `push_str("…")` literal into per-byte pushes, specialize a constant-ASCII `push` to `push_ascii_unchecked` (skipping `encode_char`'s UTF-8 width dispatch), and fuse the run of adjacent appends that leaves behind: one `internal_reserve_uninit` for the whole run, then raw byte and string writes into the space it claimed. A run-time length is read at the start of its own group and passed to the write, since the source may be the buffer itself (`buf.push_str(&buf)`) — hoisting that read over an earlier write in the same run would measure a buffer the run itself grew, so a group covers only the pieces after it.
- `value_copy_demote` — demote a deep list value-copy to a shallow spine copy when its elements are provably never mutated through the binding.
- `clone_forward` — collapse `array_clone(&array_clone(&place))` into a single clone, where inlining plus globalization left a read-only binding whose only reader is the outer clone.

There is no value-copy _elision_ pass: defensive copies are chosen at the lower phase by the ownership analysis, before NIR exists, so none are reachable from here and an imprecise one is that analysis's to fix — see [WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md), which records the standing case (a by-value `for` binding copies each element of a `List` of aggregates).

Variant and reference:

- `labeled_block_fusion` — delete the intermediate an inlined `?` helper leaves at its consumer, threading each producer directly to the value it yields. Recognises the `Option`/`Result` and the `[tag, slots…]` `sroa_variant_return` leaves in its place.
- `slot_temp_sroa` — decompose the aggregate temp an inlined helper leaves where fusion cannot relocate the consumer into the block, as in the value-producing `let x = f()?` or a two-armed `get_pow10`. Each projected slot gets a local declared ahead of the block, so its definition dominates every read, and the exits assign it instead of building the aggregate. Takes the `[tag, slots…]` tuple `sroa_variant_return` leaves, a struct literal whose reads cover every field, and an exit handing over the aggregate rather than its fields, which it binds and projects.
- `ref_elim` — drop reference bindings read only via field access, rewriting each read to the source; a shared borrow of a pure aggregate substitutes the aggregate so its projections fold.

Scalar and dataflow:

- `copy_prop` — propagate trivial copies (`let x = y`) and drop the binding. A value-type copy propagates however many times each side is read when neither binding is ever written, since the sharing is then unobservable.
- `param_spec` — interprocedural constant propagation over struct fields: clone a callee on the constant fields of a by-reference struct its caller passes, substituting those reads.
- `dae` — drop parameters never read by the callee, and the pure argument at every call site. Run to its own fixed point, since dropping one parameter can leave a caller's dead; the outer loop's iteration count would otherwise track the depth of a forwarding chain.
- `drve` — make a function void-returning when its result is dropped at every call site.
- `store_load_forward` — forward a stored literal to a later unmodified load.
- `elide_local` — drop a binding that is never read (keeping its value if impure).
- `let_block_flatten` — the value-block normal form: hoist the straight-line leading statements out of a block-tailed binding (`let x = { stmts…; tail }` → `stmts…; let x = tail`), which is the shape `sroa`'s direct-literal matcher keys on.
- `scalar_forward` — fold the inliner's leftover single-use pure-scalar value-parameter temps into their one use, so the backend emits the operand instead of a `local.set` / `local.get` round-trip.
- `const_folding` — partial evaluation: constant arithmetic (an `enum` case counts as one — it interns as the discriminant it lowers to), compile-time execution, immutable-global reads, constant-branch collapse, short-circuit simplification (a neutral operand keeps the other, an absorbing one becomes the result when the deleted operand can neither trap nor be observed), and constant struct / tuple / variant values (field projection, aggregate arguments and results of a compile-time call, and struct / tuple / variant / enum patterns over a constant scrutinee, with the arm's bindings and guard — except a binding that names storage rather than a value). A constant sequence's length and elements read out of it too, whether it is a local literal or a global. An immutable global's value is read from the assignment that fills its slot as well as from its initializer, since a non-trivial initializer is extracted into module init; a global something writes through, or hands a part of to a local, is not read at all. A compile-time call runs the callee's statements — `let` sequences, decided branches, early returns, loops, and the expression-position blocks inlining leaves — bounded by a work budget rather than by a constant trip count, and abandons the call rather than stepping past a statement it cannot perform. It also writes: a store, an element write, an allocation and a copy all land in the value the frame itself built, and a call writing through a `&mut` parameter runs and writes back into the caller's place. So a container filled at compile time — `push` and the growth it triggers included — is a compile-time value, and one whose elements are bytes leaves the engine as the literal a source string lowers to — as does a container literal still computing contents the engine already knows, which is what a value copy of a constant leaves behind. A closed block — one that builds its value in locals of its own, writes only to those, and yields the result — runs as a frame of its own, which is what folds a fully-constant string template to the literal it denotes. Only a frame may step past a write, since only a frame performs one; an ordinary walk keeps no value across a call that writes. A mutable local carries its scalar value between writes. What bounds that is the construct whose children run only sometimes: the locals it may write are dropped before each of its alternatives, so no arm folds against what the arm beside it assigned, and again after it, so nothing past it does either. An `if` whose condition the env decides is not such a construct — exactly one arm runs, so the walk enters it with the env intact and keeps what it writes, which is what folds a chain of decided branches each writing the next one's condition in a single walk rather than one link per iteration.
- `const_branch_prune` — simplify trivial blocks and fold a constant-condition `if` to its taken arm.

Loop and field:

- `licm` — hoist loop-invariant field-access chains and non-trapping arithmetic out of loops. A field load blocked only by an opaque `&mut`-call clobber of its pointee type (a may-alias, e.g. `write_escaped_string(&mut buf, &s)` where a caller could pass `buf === s`) is still hoisted, then reloaded after each clobbering statement — the clobber-free path drops the per-iteration load while an alias still sees the fresh field. An evaluation-order gate refuses when a read could observe the stale hoist.
- `condition_implication` — eliminate bounds/range checks implied false by a dominating loop guard, `if`, short-circuit, or early-exit; drop a constant-bounded index check; and, in a forward pass, drop a redundant re-check when an earlier access already proved the same index in bounds. Subsumes WIR bounds-check elimination.
- `loop_version_bce` — split a loop into a checks-deleted fast path and an unchanged slow path when a bound relation holds by per-iteration transitivity; a simple fill loop further collapses to `array.fill`.
- `tmpl_hoist` — hoist a template string's backing buffer out of a loop and reuse it when the result does not escape the iteration. It recognises an expansion by the label `synthesis::template` stamps on it, which is why `const_branch_prune` leaves that block un-flattened until the fixpoint ends.
- `field_scalarize` — shadow hot GC fields in scalar locals across a loop, with dataflow-driven write-back and re-read. A nested loop is inside that scope rather than an exit from it: only the candidates a call in its body reaches are committed before it and re-read after, and an unlabeled `break` out of it joins the loop's other exits instead of committing every scalar on the spot. A bit-buffer refill loop reaches nothing and so syncs nothing, which is 6% of `core:zlib`'s inflate.

Whole-program and backend:

- `dce` — remove unreachable functions, types, string/bytes literals, and WASI imports by call-graph reachability.
- `promote_fields` / `freeze_pure_arith` (`extract.rs`) — freeze a pure operand position into the `ValueId` it denotes. Arithmetic freezes before the loop (on the clean graph, which is what makes freezing a constant leaf read sound) and again last, after every binary-walking pass; scalar `FieldAccess` over a stable receiver freezes between them, once SROA has settled the struct shape.
- `match_to_switch` — lower a dense integer/enum `match` to a `br_table` switch, once it covers twelve values, which one range arm can do alone. The table replaces a cascade the predictor gets right with a single indirect branch, so it pays only once that cascade is long.
- `if_chain_to_match` — fuse a run of sibling `if K == x { … }` statements over one local into a single `Match`. A derived `Deserialize` routes a field through such a run, unrolled one arm per declared field and left by none of them, so a struct pays one comparison per field declared for _every_ field on the wire. The guards are exclusive because the constants are distinct and no arm writes the local; the constant bindings between the arms (the unrolled index) move ahead of the run. No width threshold of its own — the `Match` alone never tests more keys than the flat run.
- `select_lowering` — lower an `if` with pure arms to a branchless `builtin::select`.
- `multi_value_return` — emit the multi-value ABI for tuple/struct returns whose call sites destructure.
- `const_object_globalization` — hoist constant read-only aggregates, and pure calls on constants that build heap values, into shared immutable globals (see [WEP](./wep-2026-05-31-const-object-globalization.md)). A packed `Array<u8>` counts as an aggregate: it is what a `String` literal leaves once `string_push`'s fusion reads only its `repr` and SROA takes the struct away. A field reaching the aggregate through a binding of its own still hoists — that binding is what SROA leaves of a constant it split, so its definition is substituted back in rather than wrapped in a block, a block being a runtime assignment where the point is an instantiation-time constant. A borrow a builtin receives answers the read-only question from `FunctionRef::reads_param_only`, there being no body to walk. A constant a callee borrows is left alone when that callee delivers the referent back out, which would share one object across every call. Delivering it means reaching a place that outlives the borrow: handing it on as a shared-reference argument asks the same question of that callee instead, a Wasm instruction over primitives cannot keep it at all, and a local assigned from a projection is another name for the same storage rather than an escape. A hoist the later folds leave with no reader is taken back, dropping the initializer with it — unless it could trap, which is observed like any other effect.

## Lowering optimizations

NIR→WIR lowering avoids a few redundant shapes, firing once during the build at all levels — for example treating the final arm of an exhaustive match as irrefutable, and lowering a primitive-element array clone to a bulk `array.copy` rather than an interpreted per-element loop. String and bytes literals lower to a generic aggregate, so length folding, `&"…"` collapse, and globalization all reuse the aggregate machinery with no string-specific paths.

## WIR optimizations

`wir_optimize.rs` mutates the `WirPackage` in place after WIR build; phases run in order and may iterate.

1. Type representation — nullable-ref lowering; small-variant returns to multi-value.
2. Box-local elimination — substitute the field read for a `Box<T>` local lowering minted, then retype the ones adjacency cannot move to the field they wrap. A by-reference `for` bumps the index between a box's definition and its use, so nothing may move there.
3. Data flow — forward constant struct fields for constant-index bounds-check elimination.
4. Library rewrites — short-string append expansion; constant-array data promotion (only where packing encodes smaller than the inline `T.const` operands, since a data segment stores each element at full width while an operand is LEB128-compressed); large-literal splitting; elision of a whole-array zero fill on a fresh `array.new_default` (the `List::filled(n, 0)` shape).
5. Peephole — Wasm instruction-selection rewrites with no NIR analogue, including `select` for a value-producing `if` whose arms are both cheap, pure and trap-free. That last one is `nir/select_lowering`'s dual for a shape NIR never holds: `&&` / `||` stay one node until `emit_binary_wir` lowers the short-circuit to a branch.
6. Write-only local elimination — for locals only the WIR builder synthesises.
7. Global cleanup — constant-initializer promotion, identical-global dedup, and dead-data pruning.
8. Branch hints — `br_if` selection and trap-based cold/likely inference (also at `-O0`).
9. Final DCE and compaction.

A pass earns its place here only by changing the emitted Wasm. Skip-scanning
one over the benchmark, example, and fixture corpus — disabling it and diffing
the output — is what settles that; anything NIR or a sibling WIR pass already
covers leaves the bytes identical and does not belong. The exception is
`split_large_array_literals`, which scans as byte-neutral because no corpus
program reaches its bound: it is a JIT-pathology guard for >256-element
literals, not an optimization.

A `#![wasm_module(...)]` core module — the allocator — runs this same list as a package of its own, since codegen emits it verbatim. Its passes are named `wir/<module>:<pass>` so `WADO_SKIP_PASS` / `WADO_DUMP_PASS_*` address the two runs separately.

Branch hints are transparent annotations on `if`/`br_if` conditions: a pass looks through a hint when matching, drops it when eliminating the branch, and flips it when negating the condition. wasmtime lays the cold side out of line; `-f no-branch-hinting` disables the feature for benchmarking.

## Shared facilities

- `mod_ref.rs` — a conservative mod/ref summary backing the move-safety predicates (`may_clobber`, `can_move_past`).
- `arena_query.rs` — shared arena queries (purity and trap classification, mutation and place-root checks, break-target search, the promoted-read queries). The census walk itself is on `Body`, memoized per session by the engine.
- `nir_visitor.rs` — the shared pre/post-order visitor traits.

## Differential testing (EMI)

`wado-compiler/tests/emi.rs` checks the optimizer against itself: a block behind `builtin::black_box(false)` is unreachable at run time but visible to every pass, so injecting one must leave the program's output unchanged. The design is in [WEP: Compiler Fuzzing](./wep-2026-08-19-compiler-fuzzing.md).

The material comes from three roots — the e2e fixtures, the stdlib modules carrying `test` blocks, and the `example/` programs — which `WADO_EMI_ROOTS` selects among.

`mise run emi-calibrate` keeps the sources an empty guard leaves alone, writing the corpus to `target/emi/corpus.txt` and every exclusion with its reason to `target/emi/calibration.txt`. `mise run emi-mutate` then injects a payload behind the guard over that corpus, and delta-debugs a finding down to the guards that carry it under `target/emi/findings/`.

`.github/workflows/emi.yml` runs both stages nightly over `WADO_EMI_SHARD=k/n` shards.

## Not yet implemented

Missing optimizations, one entry per pass-shaped gap. Architectural work — compile speed, graph precision, the saturation end state — is tracked in [WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md) instead.

- [ ] Sparse Conditional Constant Propagation (SCCP) and interprocedural SCCP.
- [ ] Global Value Numbering across effectful nodes (pure-value hash-consing already exists in the value graph).
- [ ] Instruction combining — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`).
- [ ] Dead store elimination.
- [ ] Strength reduction; reassociation; jump threading; SimplifyCFG.
- [ ] Cross-block copy propagation.
- [ ] Sinking pure definitions into the branch that uses them (partial DCE). The
      mirror of LICM, reusing its motion-safety predicates; past effectful code
      it is sound where hoisting is not. `core:log`'s disabled path pays two
      allocations for arguments its gate discards.
- [ ] Forwarding a local bound to a global read. The graph names no global read,
      so `let s = G` reaches the use only when copy propagation removes the
      binding — never for a `String`. Naming it needs a generation check at the
      read site, the one `FieldAccess` promotion makes by version. Until then
      `remarks::collect_param_gate_remarks` reports the miss.
- [ ] Devirtualizing effect dispatch. An operation costs a global load, an
      `outer` save/restore, a `ref.cast` and a `call_ref`, none inlinable. A
      single non-self-delegating `impl` can lower to a direct call; typing each
      dispatch field precisely retires the `ref.cast` on its own.
- [ ] `param_spec` profitability — specialize only when the constants can decide
      a branch, so a chain that never folds stops duplicating code.
- [ ] Argument promotion — pass a by-reference parameter's fields by value when
      the callee only reads them, and return them by multi-value when it only
      writes them. Together they retire a scratch aggregate at its allocation
      site, which `sroa` then finishes. `sroa_param` passes one field,
      `stored_params` decides the escape precondition, and
      `multi_value_return` / `sroa_variant_return` own the write-back ABI, so
      what is missing is passing several at once under an arity cap, and
      returning the written ones.
      `param_spec` covers only the constant case; a non-constant field still
      costs a GC load per read. `core:json`'s number scanner is the standing
      case: its `ScannedNumber` is written by one callee and read by another,
      each through nothing but field access, and costs ~5% of the json-canada
      deserialize phase. Inlining that pair also buys caller-specific dead-field
      elimination, which promotion alone would not recover.
- [ ] Factoring a conjunctive if-chain into a decision tree. `if_chain_to_match`
      fuses a run whose guards are one `K == x`. A run of
      `K0 == x0 && K1 == x1 && …` could be split on the atom that discriminates
      best, then nested. That reaches the hand-written dispatchers the
      synthesised `FieldSchema::lookup` tree does not. An atom that guards
      another's operand range has to be tested first, or a miss becomes a trap.
- [ ] Tail call optimization (`return_call`).
- [ ] Bounds-check elimination for chained sequential access (`arr[0]; arr[1]; arr[2]`).
- [ ] Folding a `match` whose scrutinee is a syntactically known
      `VariantConstruct`. The constant-scrutinee path runs through
      `const_eval::Value`, which is all-or-nothing constant, so "case known,
      payload opaque" is inexpressible there.

## Tried and found ineffective

- Empty-array singleton for default `String` fields — no measurable gain; the GC allocator handles tiny zero-length arrays cheaply.
- `array.copy` for `List::grow` — several times slower than the element loop under current runtime JITs.

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).

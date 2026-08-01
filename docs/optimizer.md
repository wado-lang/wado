# Wado Optimizer

The optimizer rewrites the Normalized IR (NIR; see [WEP: NIR Layer](./wep-2026-05-11-nir.md)) in place before lowering to WIR, then runs a smaller set of WIR-level passes before Wasm emission. Pass span names used by `WADO_LIST_PASSES` / `WADO_SKIP_PASS` / `WADO_DUMP_PASS_*` carry a `nir/` or `wir/` prefix.

The module-level docs in `src/optimize.rs` and `src/wir_optimize.rs` are the authoritative pass index and ordering; per-pass design lives in each pass's source. This document is an architectural overview with a one-line summary per pass.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a complex compiler transformation — it keeps the compiler small, leverages the runtime JIT, and produces smaller output (`select` for branchless conditionals, `array.copy`/`array.fill` for bulk ops, `br_table` for dense matches).

## Optimization levels

All levels run DCE on functions, types, and globals.

| Flag            | Iterations | Inline threshold | Notes                                             |
| --------------- | ---------- | ---------------- | ------------------------------------------------- |
| `-O0`           | 0          | N/A              | DCE only + `match_to_switch` + post-loop rewrites |
| `-O1`           | 2          | 4                |                                                   |
| `-O2` (default) | 10         | 13               |                                                   |
| `-O3`           | 30         | 32               |                                                   |
| `-Os`           | 10         | 13               | strips the Wasm name section                      |

The fixed-point loop exits early on convergence. The backend-required rewrites (`select_lowering`, `multi_value_return`) and `match_to_switch` run at every level, including `-O0`.

## Architecture

### Live value graph

Pure values are the optimizer's source of truth, not re-derived per pass. Each operand position is either a skeleton subtree or a promoted pure value interned in a per-function pool, hash-consed so congruent values share one node. The graph is built once per function and maintained in place across passes via e-class union, never rebuilt — so pure-value CSE falls out of the pool, constant folding reads pooled values, and bounds-check elimination recognises them structurally. See [WEP: The Live ValueGraph](./wep-2026-06-15-live-value-graph.md).

### Worklist rewrite engine

Genuinely-local NIR rewrites run as rules on a worklist engine over one function's arena: a node is revisited only when an edit may have made it reducible, rather than via repeated whole-tree sweeps. The engine owns the session state and a mutating edit API that keeps it coherent. Flow-sensitive passes that need per-block dataflow keep their own walkers. See [WEP: NIR Rewrite Engine](./wep-2026-06-05-nir-rewrite-engine-design.md).

### Unified peephole session

The position-flexible local rules run together over one engine session per function, interleaved on a single worklist. It runs twice per iteration — before and after `inline` — so each rule sees the instruction window the other exposes.

### Per-function dirty-set gating

A function gate lets every loop pass skip functions unchanged since it last ran; interprocedural passes still scan all functions but report only the ones they touched. Gating affects only which functions a pass visits, never the IR a visit produces, so an imprecise gate can cost optimization quality (a missed rewrite) but never correctness.

## Pipeline

`optimize.rs` orchestrates the NIR stages; `wir_optimize.rs` runs the WIR stages.

1. Early DCE — remove unreachable functions/types/globals.
2. Fixed-point loop (skipped at `-O0`): container SROA, peephole (pre-inline), value-copy demotion, parameter SROA, inlining, peephole (post-inline), SROA, copy propagation, dead-argument and dead-return elimination, constant folding, parameter specialization, LICM, template hoisting.
3. Post-loop, once: field scalarization, store-load forwarding, template-wrapper cleanup, constant-object globalization, and a final folding pass.
4. Final DCE.
5. Backend-required rewrites (all levels): select lowering, multi-value returns.
6. WIR-level passes — see [WIR optimizations](#wir-optimizations).

## NIR passes

Allocation and aggregate:

- `inline` — replace calls to small, non-recursive functions with their body; reference parameters and receivers inline too. `#[inline]` raises the size threshold, `#[inline(always)]` forces it, `#[inline(never)]` and cold call sites opt out.
- `sroa` — decompose non-escaping struct/tuple locals into scalar locals. The highest-impact WasmGC pass.
- `container_sroa` — turn `List<Struct>` / `List<Tuple>` into parallel per-field lists (array-of-structs → struct-of-arrays).
- `sroa_param` — replace a single-field-struct reference parameter with its inner scalar, unwrapping the box that `&T` values allocate.
- `elide_box_local` — collapse a box bound once and read once into its inner value.
- `array_literal` — fold an array-builder window into a single fixed-array literal.
- `value_copy_demote` — demote a deep list value-copy to a shallow spine copy when its elements are provably never mutated through the binding.

There is no value-copy _elision_ pass: defensive copies are inserted precisely at the lower phase by the ownership analysis, so none exist for an elider to recover (see [WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md)).

Variant and reference:

- `labeled_block_fusion` — delete the intermediate `Option`/`Result` an inlined `?` helper leaves at its consumer, threading each producer directly to the value it yields.
- `ref_elim` — drop reference bindings read only via field access, rewriting each read to the source; a shared borrow of a pure aggregate substitutes the aggregate so its projections fold.

Scalar and dataflow:

- `copy_prop` — propagate trivial copies (`let x = y`) and drop the binding.
- `param_spec` — interprocedural constant propagation over struct fields: clone a callee on the constant fields of a by-reference struct its caller passes, substituting those reads.
- `dae` — drop parameters never read by the callee, and the pure argument at every call site.
- `drve` — make a function void-returning when its result is dropped at every call site.
- `store_load_forward` — forward a stored literal to a later unmodified load.
- `elide_local` — drop a binding that is never read (keeping its value if impure).
- `const_folding` — partial evaluation: constant arithmetic, compile-time execution, immutable-global reads, constant-branch collapse, short-circuit simplification (a neutral operand keeps the other, an absorbing one becomes the result when the deleted operand can neither trap nor be observed), and constant struct / tuple values (field projection, aggregate arguments and results of a compile-time call, and struct / tuple patterns over a constant scrutinee, with the arm's bindings and guard). A constant sequence's length and elements read out of it too, whether it is a local literal or a global. An immutable global's value is read from the assignment that fills its slot as well as from its initializer, since a non-trivial initializer is extracted into module init; a global something writes through, or hands a part of to a local, is not read at all. A compile-time call runs the callee's statements — `let` sequences, decided branches, early returns, loops, and the expression-position blocks inlining leaves — bounded by a work budget rather than by a constant trip count, and abandons the call rather than stepping past a statement it cannot perform. It also writes: a store, an element write, an allocation and a copy all land in the value the frame itself built, and a call writing through a `&mut` parameter runs and writes back into the caller's place. So a container filled at compile time — `push` and the growth it triggers included — is a compile-time value, and one whose elements are bytes leaves the engine as the literal a source string lowers to. A closed block — one that builds its value in locals of its own, writes only to those, and yields the result — runs as a frame of its own, which is what folds a fully-constant string template to the literal it denotes. Only a frame may step past a write, since only a frame performs one; an ordinary walk keeps no value across a call that writes.
- `const_branch_prune` — simplify trivial blocks and fold a constant-condition `if` to its taken arm.

Loop and field:

- `licm` — hoist loop-invariant field-access chains and non-trapping arithmetic out of loops. A field load blocked only by an opaque `&mut`-call clobber of its pointee type (a may-alias, e.g. `write_escaped_string(&mut buf, &s)` where a caller could pass `buf === s`) is still hoisted, then reloaded after each clobbering statement — the clobber-free path drops the per-iteration load while an alias still sees the fresh field. An evaluation-order gate refuses when a read could observe the stale hoist.
- `condition_implication` — eliminate bounds/range checks implied false by a dominating loop guard, `if`, short-circuit, or early-exit; drop a constant-bounded index check; and, in a forward pass, drop a redundant re-check when an earlier access already proved the same index in bounds. Subsumes WIR bounds-check elimination.
- `loop_version_bce` — split a loop into a checks-deleted fast path and an unchanged slow path when a bound relation holds by per-iteration transitivity; a simple fill loop further collapses to `array.fill`.
- `tmpl_hoist` — hoist a template string's backing buffer out of a loop and reuse it when the result does not escape the iteration.
- `field_scalarize` — shadow hot GC fields in scalar locals across a loop, with dataflow-driven write-back and re-read.

Whole-program and backend:

- `dce` — remove unreachable functions, types, string/bytes literals, and WASI imports by call-graph reachability.
- `match_to_switch` — lower a dense integer/enum `match` to a `br_table` switch.
- `select_lowering` — lower an `if` with pure arms to a branchless `builtin::select`.
- `multi_value_return` — emit the multi-value ABI for tuple/struct returns whose call sites destructure.
- `const_object_globalization` — hoist constant read-only aggregates, and pure calls on constants that build heap values, into shared immutable globals (see [WEP](./wep-2026-05-31-const-object-globalization.md)).

## Lowering optimizations

NIR→WIR lowering avoids a few redundant shapes, firing once during the build at all levels — for example treating the final arm of an exhaustive match as irrefutable, and lowering a primitive-element array clone to a bulk `array.copy` rather than an interpreted per-element loop. String and bytes literals lower to a generic aggregate, so length folding, `&"…"` collapse, and globalization all reuse the aggregate machinery with no string-specific paths.

## WIR optimizations

`wir_optimize.rs` mutates the `WirPackage` in place after WIR build; phases run in order and may iterate.

1. Type representation — nullable-ref lowering; small-variant returns to multi-value.
2. Struct-local elimination — substitute field reads for single-field struct and box locals.
3. Data flow — forward constant struct fields for constant-index bounds-check elimination.
4. Library rewrites — short-string append expansion; constant-array data promotion; large-literal splitting.
5. Peephole — Wasm instruction-selection rewrites with no NIR analogue; multi-field struct elimination; nullability-driven rewrites (elide redundant `ref.as_non_null`, fold `ref.is_null` on a non-null reference).
6. Write-only local elimination — for locals only the WIR builder synthesises.
7. Global cleanup — constant-initializer promotion, identical-global dedup, and dead-data pruning.
8. Branch hints — `br_if` selection and trap-based cold/likely inference (also at `-O0`).
9. Final DCE and compaction.

Branch hints are transparent annotations on `if`/`br_if` conditions: a pass looks through a hint when matching, drops it when eliminating the branch, and flips it when negating the condition. wasmtime lays the cold side out of line; `-f no-branch-hinting` disables the feature for benchmarking.

## Shared facilities

- `mod_ref.rs` — a conservative mod/ref summary backing the move-safety predicates (`may_clobber`, `can_move_past`).
- `arena_query.rs` — shared arena queries (purity and trap classification, mutation and place-root checks, break-target search).
- `nir_visitor.rs` — the shared pre/post-order visitor traits.

## Not yet implemented

- [ ] Sparse Conditional Constant Propagation (SCCP) and interprocedural SCCP.
- [ ] Global Value Numbering across effectful nodes (pure-value hash-consing already exists in the value graph).
- [ ] Instruction combining — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`).
- [ ] Dead store elimination.
- [ ] Strength reduction; reassociation; jump threading; SimplifyCFG.
- [ ] Cross-block copy propagation.
- [ ] `param_spec` profitability — specialize only when the constants can decide
      a branch, so a chain that never folds stops duplicating code.
- [ ] Argument promotion — pass a by-reference parameter's fields by value when
      the callee only reads them. `param_spec` covers the constant case; a
      non-constant field still costs a GC load per read.
- [ ] Tail call optimization (`return_call`).
- [ ] Bounds-check elimination for chained sequential access (`arr[0]; arr[1]; arr[2]`).

## Tried and found ineffective

- Empty-array singleton for default `String` fields — no measurable gain; the GC allocator handles tiny zero-length arrays cheaply.
- `array.copy` for `List::grow` — several times slower than the element loop under current runtime JITs.

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).

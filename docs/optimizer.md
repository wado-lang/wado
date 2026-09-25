# Wado Optimizer

The optimizer rewrites NIR ([WEP: NIR](./wep-2026-05-11-nir.md)), then a smaller
set of passes rewrites WIR before emission. This document lists what runs, one
line per pass. The order is the one in `src/optimize.rs` and
`src/wir_optimize.rs`, and `WADO_LIST_PASSES` prints it. Each pass's module doc
says how it works.

## Philosophy

When WebAssembly provides a native instruction for a feature, prefer it over a
complex compiler transformation. That keeps the compiler small, lets the runtime
JIT do the work, and produces smaller output. Examples are `select` for
branchless conditionals, `array.copy` and `array.fill` for bulk operations, and
`br_table` for dense matches.

## Optimization Levels

| Flag            | Iterations | Inline threshold | Notes                                                 |
| --------------- | ---------- | ---------------- | ----------------------------------------------------- |
| `-O0`           | 0          | N/A              | Part of the pipeline; see [Pipeline](#pipeline)       |
| `-O1`           | 2          | 4                |                                                       |
| `-O2` (default) | 15         | 16               |                                                       |
| `-O3`           | 20         | 26               |                                                       |
| `-Os`           | 15         | 16               | Strips the name section; a failed `assert` only traps |

The inline threshold bounds what one inlined copy adds to its caller. That is
the Wasm instructions on the callee's hot path, less the call it replaces.
`--optimize-iterations` and `--optimize-inline-threshold` override a level's
values. `--optimize-inline-growth <pct>` also bounds how far inlining may grow
the whole program. No level sets it.

A pass reports a change only when it made one, because the fixed-point loop
stops once no pass reports a change. At `-O2`, `-O3`, and `-Os` the loop must
converge within the level's own count. At `-O1`, and wherever
`--optimize-iterations` sets the count, it is only a budget.

## Architecture

NIR has two tiers: a skeleton carrying effect order, control flow, and
allocation, and a hash-consed graph of the pure values it reaches. Local
rewrites are rules on one worklist engine, and a per-function dirty set lets a
pass skip functions unchanged since it last ran. CSE, GVN, and pure copy
propagation are not passes, because the hash-consing already does them. See
[WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md).

## Pipeline

`-O0` runs only DCE, `match_to_switch`, step 7, and the part of step 8 that
[WIR Passes](#wir-passes) keeps at `-O0`. Every other level runs the whole list.

1. DCE.
2. Before the loop: `cold_outline`, `match_to_switch` over global initializers,
   and the early arithmetic promotion.
3. The fixed-point loop: `container_sroa`, `peephole`, `value_copy_demote`,
   `sroa_param`, `sroa_variant_return`, `inline`, `peephole`,
   `let_block_flatten`, `sroa`, `copy_prop`, `dae`, `drve`, `const_folding`,
   `param_spec`, `licm` (with `condition_implication`), `tmpl_hoist`. Once it
   converges, `inline` releases the callees it held back, and the loop runs
   again.
4. After the loop, once: `field_scalarize`, `store_load_forward`,
   `const_branch_prune`, `const_object_globalization`, `const_folding`,
   `scalar_forward`, `clone_forward`.
5. DCE.
6. `promote_fields`, `condition_implication` (with `const_branch_prune`),
   `loop_version_bce`.
7. Backend rewrites: `select_lowering`, `multi_value_return`,
   `multi_value_param`, `freeze_pure_arith`.
8. The WIR passes.

## NIR Passes

`peephole` is one worklist per function shared by the rules that need no fixed
position. It runs once before `inline` and once after, and each run has rules of
its own:

- Both runs: `aggregate_forward`, `const_branch_prune`, part of
  `const_folding`, `elide_local`, `identity_cast`, `known_case`,
  `string_push`, and `tuple_projection`.
- Before `inline` only: `if_chain_to_match`, `match_to_bitset`, and
  `match_to_switch`, so a copied body is already lowered.
- After `inline` only: `closure_devirt`, `drop_value`, `elide_box_local`,
  `labeled_block_fusion`, `ref_elim`, and `slot_temp_sroa`. Each cleans up a
  shape that inlining leaves behind.

Allocation and aggregates:

- `inline` — replace a call to a small, non-recursive function with its body.
  `#[inline]`, `#[inline(always)]`, and `#[inline(never)]` adjust the decision,
  and a cold call site opts out.
- `cold_outline` — move the region a `cold_path()` marks into a function of its
  own.
- `sroa` — split a struct, tuple, or array local used only for element access
  into scalar locals. The highest-impact WasmGC pass.
- `container_sroa` — turn a `List` of structs or tuples into one list per field.
- `sroa_param` — pass the one field a callee reads instead of the struct.
- `sroa_variant_return` — return a variant as a `[tag, slots…]` tuple
  ([WEP: Variant Return Scalarization](./wep-2026-08-03-variant-return-abi.md)).
- `elide_box_local` — collapse a box bound once and read once into its value.
- `drop_value` — keep only the effects of a labeled block whose value is
  discarded.
- `string_push` — specialize a constant ASCII push, and reserve a run of
  appends at once.
- `value_copy_demote` — make a deep list copy shallow when its elements are
  never mutated.
- `clone_forward` — collapse a clone of a clone into one.

Lower's ownership analysis chooses the defensive copies before NIR exists, so no
pass here elides them
([WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md)).

Variants and references:

- `labeled_block_fusion` — remove the `Option` or `Result` an inlined helper
  builds only for its caller to test, as `?` and `if let` do.
- `slot_temp_sroa` — split the aggregate an inlined helper leaves where fusion
  cannot reach.
- `closure_devirt` — call a closure directly when its callee is known.
- `ref_elim` — replace a reference read only through fields with its source.
- `aggregate_forward` — hand a freshly built aggregate to its consumer directly.
- `known_case` — decide a `match` over a variant whose case is known.
- `tuple_projection` — `[a, b, c].1` → `b`.
- `identity_cast` — drop a cast between types that share one representation.

Scalars and dataflow:

- `copy_prop` — propagate a trivial copy and drop the binding.
- `param_spec` — specialize a callee on the constant arguments its callers
  pass.
- `dae` — drop a parameter the callee never reads.
- `drve` — drop a return value every caller discards.
- `store_load_forward` — forward a stored value to a later load.
- `elide_local` — drop a binding that is never read.
- `let_block_flatten` — hoist the statements out of a block-valued binding.
- `scalar_forward` — fold a single-use scalar temp into its use.
- `const_folding` — partial evaluation: constant arithmetic, compile-time calls,
  constant globals, and constant aggregates
  ([WEP: NIR Interpreter](./wep-2026-04-27-nir-interpreter.md)).
- `const_branch_prune` — simplify trivial blocks, and take the constant side of
  an `if`.

Loops and fields:

- `licm` — hoist loop-invariant field reads and arithmetic out of a loop.
- `condition_implication` — drop a bounds or range check a dominating
  condition already decides.
- `loop_version_bce` — split a loop into a check-free fast path and the
  original, and turn a fill loop into `array.fill`.
- `tmpl_hoist` — reuse a template string's buffer across loop iterations.
- `field_scalarize` — keep hot GC fields in locals across a loop.

Whole program and backend:

- `dce` — remove unreachable functions, types, globals, literals, and imports.
- `promote_fields` / `freeze_pure_arith` — fold pure field reads and arithmetic
  into the value graph.
- `match_to_bitset` — test membership in a set of literals and ranges with one
  bit mask.
- `match_to_switch` — lower a dense integer or enum `match` to `br_table`.
- `if_chain_to_match` — fuse a run of `if K == x` statements into one `match`.
- `select_lowering` — lower an `if` whose arms are pure and cannot trap to
  `select`.
- `multi_value_return` — return a tuple or struct as one Wasm result per field.
- `multi_value_param` — pass an aggregate read only by field as one parameter
  per field.
- `const_object_globalization` — share a constant aggregate as an immutable
  global ([WEP](./wep-2026-05-31-const-object-globalization.md)).

## WIR Passes

A WIR pass stays only if it changes the emitted Wasm. One that NIR or another
WIR pass already covers is removed. At every level, nullable references are
lowered first. `-O0` then only infers branch hints and removes dead items.

1. Trivial copy propagation.
2. Box-local elimination.
3. Constant struct-field forwarding, for constant-index bounds checks.
4. Array rewrites: constant arrays to data segments, large literals split, and
   the zero fill of a fresh array removed.
5. Peephole: repeated field loads reused, instruction selection (`select`,
   rotates), and variant result slots flattened.
6. Write-only local elimination.
7. Global cleanup (constant initializers, duplicate globals, dead data), then
   `br_if` selection and branch hints.
8. DCE and compaction.

A `#![wasm_module(...)]` core module, such as the allocator, runs the same list
on its own, with its passes named `wir/<module>:<pass>`.

wasmtime lays out the cold side of a hinted branch out of line.
`-f no-branch-hinting` disables hints for benchmarking.

## Differential Testing (EMI)

`wado-compiler/tests/emi.rs` injects code behind a guard that is always false at
run time, and checks that the program's output does not change at any level.
`mise run emi-calibrate` and `mise run emi-mutate` run it, and CI runs it
nightly.
See [WEP: Compiler Fuzzing](./wep-2026-08-19-compiler-fuzzing.md).

## Not Yet Implemented

Architectural work is tracked in
[WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md).

- [ ] Sparse conditional constant propagation, intra- and interprocedural.
- [ ] Global value numbering across effectful nodes.
- [ ] Instruction combining over identical operands (`x - x`, `x & x`, `~(~x)`).
- [ ] Dead store elimination.
- [ ] Strength reduction, reassociation, jump threading, and CFG simplification.
- [ ] Cross-block copy propagation.
- [ ] Sinking a pure definition into the branch that uses it.
- [ ] Forwarding a local bound to a global read.
- [ ] Devirtualizing effect dispatch.
- [ ] Specializing in `param_spec` only where a constant decides a branch.
- [ ] Compile-time evaluation through a `&mut` held in a struct field.
- [ ] Argument promotion: passing a by-reference parameter's fields by value.
- [ ] Factoring a conjunctive `if` chain into a decision tree.
- [ ] Scalarizing a struct borrowed by `&mut`.
- [ ] Tail calls (`return_call`).
- [ ] Bounds-check elimination across sequential accesses (`a[0]; a[1]; a[2]`).
- [ ] Folding an effect-free call on constants whose callee exceeds the inline
      budget.
- [ ] An array literal's length as a known constant.

## Tried and Found Ineffective

- An empty-array singleton for default `String` fields: no measurable gain.

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).

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

A call to a function that declares no effects, or to an `#[ambient]` one, may be
removed when its result is unused. A call that may trap or may not return stays,
since removing it changes what the program does.

## Optimization Levels

| Flag  | Iterations | Inline threshold | Contract checks | Notes                                                                    |
| ----- | ---------- | ---------------- | --------------- | ------------------------------------------------------------------------ |
| `-O0` | 0          | N/A              | on              | Part of the pipeline; see [Pipeline](#pipeline)                          |
| `-O1` | 2          | 4                | on              | The default of `wado run` and `wado serve`                               |
| `-O2` | 15         | 16               | off             | The default of every other subcommand                                    |
| `-O3` | 20         | 26               | off             |                                                                          |
| `-Os` | 15         | 16               | off             | Strips the name section; a failed `assert` only traps; no loop unrolling |

The levels differ in what a program does, not only in its speed: below `-O2`, a
call marked `unchecked` traps when it breaks its contract (see
[Contract Checks](./spec-assertions.md#contract-checks)).

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
pass skip functions unchanged since it last ran. A whole-program analysis keeps
what each body answers alone across rounds, and re-derives it only for the
bodies a rewrite reached. CSE, GVN, and pure copy propagation are not passes,
because the hash-consing already does them. See
[WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md).

The passes run on `--optimize-threads` threads, and the output does not depend
on how many. See [WEP: Parallel Optimizer](./wep-2026-10-08-parallel-optimizer.md).

## Pipeline

`-O0` runs only DCE, `match_to_switch`, step 7, and the part of step 8 that
[WIR Passes](#wir-passes) keeps at `-O0`. Every other level runs the whole list.

1. DCE.
2. Before the loop: `cold_outline`, `match_to_switch` over global initializers,
   and the early arithmetic promotion.
3. The fixed-point loop: `container_sroa`, `peephole`, `value_copy_demote`,
   `sroa_param`, `sroa_variant_return`, `inline`, `peephole`,
   `let_block_flatten`, `sroa`, `copy_prop`, `dae`, `drve`, `const_folding`,
   `param_spec`, `licm` (with `condition_implication`). Once it
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
  and a cold call site opts out, as does every site in a function only cold
  sites reach, unless the body is no bigger than the call. A callee that
  reaches no safepoint (no call,
  no allocation) is admitted at four times the threshold where the splice pays
  back: at its only call site, or at its only site in a loop. Only sites in a
  function an entry reaches count. A function only
  loops reach runs once per iteration, so its body counts as a loop's. A call
  is tried before its arguments, and a spliced body, arguments included, is
  scanned again for calls to splice, wherever the call it replaced sat.
- `cold_outline` — move the region a `cold_path()` marks into a function of its
  own.
- `sroa` — split into scalar locals a struct, tuple, or array local used only
  for element access, or a local bound to a reference to a struct or tuple
  literal. The highest-impact WasmGC pass.
- `container_sroa` — turn a `List` of structs or tuples into one list per field.
- `sroa_param` — pass the one field a callee reads instead of the struct, by
  value or by reference.
- `sroa_variant_return` — return a variant, or hold a variant local, as a
  `[tag, slots…]` tuple ([WEP: Variant Return Scalarization](./wep-2026-08-03-variant-return-abi.md)).
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
- `identity_cast` — drop a cast between types that share one representation,
  and fold `*&e` to `e`.

Scalars and dataflow:

- `copy_prop` — propagate a trivial copy and drop the binding.
- `param_spec` — specialize a callee on the constant arguments its callers
  pass, and on the constant fields of a struct they pass by reference, a
  reference to a struct literal included.
- `dae` — drop a parameter the callee never reads.
- `drve` — drop a return value every caller discards.
- `store_load_forward` — forward a stored value to a later load.
- `elide_local` — drop a binding that is never read.
- `let_block_flatten` — hoist the statements out of a block a statement
  evaluates first: a binding's value, a call's leading argument, a condition.
  Then the one assignment of a local no `let` binds becomes its `let`, where
  every mention of the local follows it in its block.
- `scalar_forward` — fold a single-use scalar temp into its use.
- `const_folding` — partial evaluation: constant arithmetic, compile-time calls,
  constant globals, and constant aggregates
  ([WEP: NIR Interpreter](./wep-2026-04-27-nir-interpreter.md)).
- `const_branch_prune` — simplify trivial blocks, and take the constant side of
  an `if`.

Loops and fields:

- `licm` — hoist loop-invariant field reads and arithmetic out of a loop.
- `condition_implication` — drop a bounds or range check a dominating
  condition already decides, or the half of a range check it decides. A later
  check on the same path, with nothing observable between, moves into an
  earlier one: `a[i]; a[i + 1]; a[i + 2]` checks `i < len - 2` once.
- `loop_version_bce` — split a loop into a check-free fast path and the
  original, and turn a fill loop into `array.fill`.
- `unsigned_range_check` — fuse a remaining `0 <= i && i < n` into one unsigned
  compare. It runs after the passes above, which read the two halves apart.
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
  per field. A `&S` qualifies where nothing the call runs writes an `S` it did
  not allocate. It passes only the fields the callee reads on any path, loaded
  at the call after every other argument. A parameter handed on whole to
  another such parameter of the same shape qualifies with it, and takes the
  fields both read.
- `const_object_globalization` — share a constant aggregate as an immutable
  global ([WEP](./wep-2026-05-31-const-object-globalization.md)).

## WIR Passes

A WIR pass stays only if it changes the emitted Wasm. One that NIR or another
WIR pass already covers is removed. At every level, nullable references are
lowered first. `-O0` then only infers branch hints, removes dead items, and
settles the locals (step 11).

1. Trivial copy propagation.
2. Box-local elimination.
3. Constant struct-field forwarding, for constant-index bounds checks.
4. Array rewrites: constant arrays to data segments, large literals split, and
   the zero fill of a fresh array removed.
5. Peephole: repeated field loads reused, instruction selection (`select`,
   rotates), and variant result slots flattened. Then a resource whose
   `$Resource` box no reference reaches is kept as its bare handle, across the
   locals, parameters and results it moves through.
6. Write-only local elimination.
7. Global cleanup (constant initializers, duplicate globals, dead data), then
   `br_if` selection, the calls to a function left empty and the once-flags
   guarding them, loop unrolling, and branch hints. A small loop that reads GC
   arrays gains a copy running several iterations per test of its condition,
   so wasmtime shares part of each `array.get`'s bounds check across them. The
   original loop runs what is left.
8. DCE and compaction.
9. Local coalescing: a local read once is first forwarded into its read. Then
   locals of one Wasm type whose live ranges never overlap share a slot. A
   local's nullability is settled before it shares one.
10. Tee fusion: a `local.set` and the read right after it become a
    `local.tee`. A local this leaves unread is removed.
11. Local settling: a non-null local that Wasm's block-scoped rule sees read
    before any write is declared nullable. The locals are then grouped by Wasm
    type, the most used first.

A `#![wasm_module(...)]` core module, such as the allocator, runs the same list
on its own, with its passes named `wir/<module>:<pass>`.

## Branch Hints

A cold marker, `builtin::cold_path()`, says that the code around it rarely runs.
The author writes one where they expect a path to be rare. The compiler writes
one where the program itself makes a path rare: ahead of a trap, past a global's
once-guard, and in the default arm of a switch.

### What a Marker Covers

A marker makes its basic block cold. A basic block is a straight run of code
that control enters only at its start and leaves only at its end. Wado's IR
nests blocks rather than listing basic blocks, so the optimizer reads the same
rule through dominance: a point is cold when every path through it passes the
marker. The code after the marker in its block is cold, and so is the code
before it, back to the last statement that can leave the block first.

- In `if bad { let m = describe(x); panic(m); }`, every path through `describe`
  reaches the marker reify puts ahead of `panic`, so `describe` is cold too.
- In `if let Some(v) = fast(k) { return v; } cold_path(); return slow(k);`, the
  guard can return before the marker, so the guard stays hot and only what
  follows it is cold.
- In `if c { if d { cold_path(); a } else { cold_path(); b } }`, every path
  through the outer arm passes one of the two markers, so the outer arm is cold
  as a whole.

Every pass that reads a marker reads this one rule: the inliner's prices,
`cold_outline`'s regions, the callees only cold code calls, the remarks, and the
branch hints.

### Hints and Flags

A branch whose one side is cold gets a hint that the other side is likely.
wasmtime lays out the cold side out of line. A branch to a trap is hinted the
same way, marker or not.

`-f no-branch-hinting` leaves the hints out of the Wasm, so a benchmark measures
what they are worth to the engine. `-f no-cold-paths` drops every marker, the
compiler's included, before any pass reads one, so a benchmark measures what the
markers are worth to the whole pipeline.

## Profile-Guided Optimization

Not implemented yet; see
[WEP: Profile-Guided Optimization](./wep-2026-10-25-profile-guided-optimization.md).

A package commits a profile, `pgo/default.pgo`, that `wado test --pgo` records
from its `#[workload]` tests. A compile marks each outermost region the
workloads never ran with `cold_path()`, so every pass that reads a marker
treats it as it treats one the author wrote. The test world reads no profile unless `-f pgo`
asks. `-f no-pgo` ignores the profile, and `-f no-cold-paths` ignores every
marker, so three builds of one program can be compared.

Two questions stay open while profiles spread, and each optimizer change that
touches cold paths answers them again:

- What a profile gains: `benchmark/` compared across the three builds.
- What cold code costs the compile: every pass still runs on a cold function,
  so part of the compile goes to code that rarely runs. The span trace in the
  `profiling-wado-compiler` skill measures the share.

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
- [ ] Scalarizing a struct referenced by `&mut`.
- [ ] Tail calls (`return_call`).
- [ ] Folding an effect-free call on constants whose callee exceeds the inline
  budget.
- [ ] Removing an unused call by its callee's declared effects, without
  inlining it first.
- [ ] Pricing a splice by what its call site keeps: one arm of a dispatch
  every site decides (`$hole_fmt`), or one element of a returned tuple
  (`default_slot`). Both carry a synthesized `#[inline(always)]` until then.
- [ ] An array literal's length as a known constant.

## Tried and Found Ineffective

- An empty-array singleton for default `String` fields: no measurable gain.

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).

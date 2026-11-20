# Wado Optimizer

The optimizer rewrites NIR ([WEP: NIR](./wep-2026-05-11-nir.md)), then a smaller
set of passes rewrites WIR before emission. This document says what each pass
does, one section per pass, in the order `src/optimize.rs` and
`src/wir_optimize.rs` run them. A heading is the name `WADO_LIST_PASSES` prints
and `WADO_SKIP_PASS` takes, without its `nir/` or `wir/` prefix. How a pass
works is its module doc's to say.

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
| `-O0` | 0          | N/A              | on              | DCE alone, which keeps an e2e run cheap; nothing else is optimized       |
| `-O1` | 2          | 6                | on              | The default of `wado run`, `wado serve`, `wado test` and each `run-*`    |
| `-O2` | 15         | 24               | off             | For a release build; the default of every other subcommand               |
| `-O3` | 20         | 39               | off             | For fuzzing the optimizer, more than for a build                         |
| `-Os` | 15         | 16               | off             | Strips the name section; a failed `assert` only traps; no loop unrolling |

The levels differ in what a program does, not only in its speed: below `-O2`, a
call marked `unchecked` traps when it breaks its contract (see
[Contract Checks](./spec-assertions.md#contract-checks)).

The inline threshold bounds what one inlined copy adds to its caller. That is
the Wasm instructions on the callee's hot path, less the call it replaces.
`--optimize-iterations` and `--optimize-inline-threshold` override a level's
values. `--optimize-inline-growth <pct>` also bounds how far inlining may grow
the whole program. No level sets it.

Specializing a callee on its constant arguments mints a clone per distinct set
of constants, up to a cap per module. `--optimize-specializations <n>` sets the
cap. Reaching it is a remark under `--log-level info`, naming the first callee
that kept its general version.

A pass reports a change only when it made one, because the fixed-point loop
stops once no pass reports a change. At `-O2`, `-O3`, and `-Os` the loop must
converge within the level's own count. At `-O1`, and wherever
`--optimize-iterations` sets the count, it is only a budget.

## Architecture

NIR has two tiers: a skeleton carrying effect order, control flow, and
allocation, and a pool of hash-consed pure values that some of its operands
name. A pass skips a function unchanged since it last ran. See
[WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md).

The passes run on `--optimize-threads` threads, and the output does not depend
on how many. See [WEP: Parallel Optimizer](./wep-2026-10-08-parallel-optimizer.md).

Of the NIR passes, `-O0` runs only DCE, and of the WIR passes, what
[WIR Passes](#wir-passes) says it keeps. Every other level runs them all.

## Lowering

Lowering makes a few decisions an optimizer would otherwise make. Each is named
like a pass, so `WADO_SKIP_PASS` can turn it off.

### `lower/value_copy/move`

A value-semantics local at its last use hands over its storage instead of a
copy ([WEP: Ownership Analysis](./wep-2026-05-21-resource-ownership.md)).

### `lower/value_copy/owned`

A fresh value, such as a call's result or a literal, is never copied.

### `lower/value_copy/share`

A binding of an immutable local nothing moves shares its storage.

### `lower/value_copy/confined`

A by-value argument skips its copy where the callee keeps nothing of it and
nothing writes it before the call returns.

### `lower/value_copy/pass_through`

A builtin that hands an argument back, such as `builtin::select`, copies its
result only where the caller can still reach the argument.

### `value_pool/fold_const`

The value pool folds an operation on constants as it records it, in lowering
and in every NIR pass.

### `wir_build/exhaustive_last_arm`

The last arm of an exhaustive `match` tests nothing.

### `wir_build/branch_hints`

A branch to a cold path gets a hint, as [Branch Hints](#branch-hints) says.

### `wir_build/elide_terminator_unreachable`

No `unreachable` follows an instruction that already leaves the block.

### `synthesis/cm_lift_bulk`

A component binding lifts a `list` of numbers out of linear memory in one copy.

## NIR Passes

### Before the Loop

#### DCE

Remove unreachable functions, types, globals, literals, and imports. It runs
again after the loop, and no `WADO_SKIP_PASS` name skips it.

#### `dead_field`

Remove a struct field no code reads, from its type and from every literal,
assignment and pattern that names it. A removed field's initializer still runs
when it has an effect.

#### `cold_outline`

Move the region a `cold_path()` marks into a function of its own, so the hot
function inlines at its hot size.

#### `match_to_switch_globals`

[`match_to_switch`](#match_to_switch) over global initializers.

### The Fixed-Point Loop

The loop runs these passes in this order until none reports a change. Then
`inline` releases the callees it held back, and the loop runs again.

#### `drop_unreachable`

Drop the functions the previous round left unreachable, such as a callee
inlined into every caller. Not run in the first round.

#### `container_sroa`

Turn a `List` of structs or tuples into one list per field.

#### `peephole`

The local rewrites that need no fixed position, as one session per function. It
runs once before `inline` and once after. Each rule is named
`nir/peephole/<rule>`.

##### `if_chain_to_match`

Fuse a run of `if K == x` statements into one `match`. Before `inline` only.

##### `match_to_bitset`

Test membership in a set of literals and ranges with one bit mask. Before
`inline` only.

##### `match_to_switch`

Lower a dense integer or enum `match` to `br_table`. Before `inline` only.

##### `ref_elim`

Replace a reference read only through fields, or handed whole to a call, with
its source. After `inline` only.

##### `elide_box_local`

Collapse a box bound once and read once into its value. After `inline` only.

##### `labeled_block_fusion`

Remove the `Option` or `Result` an inlined helper builds only for its caller to
test, as `?` and `if let` do. After `inline` only.

##### `slot_temp_sroa`

Split the variant an inlined helper or a branching expression leaves where
fusion cannot reach. After `inline` only.

##### `drop_value`

Keep only the effects of a labeled block whose value is discarded. After
`inline` only.

##### `closure_devirt`

Call a closure directly when its callee is known. After `inline` only.

##### `known_case`

Decide a `match` over a variant whose case is known.

##### `identity_cast`

Drop a cast between types that share one representation.

##### `identity_deref`

Fold `*&e` to `e`.

##### `field_through_ref`

Fold `(&e).f` to `e.f`.

##### `aggregate_forward`

Hand a freshly built aggregate to its consumer directly.

##### `elide`

Drop a binding, or a store to a local, that no read sees.

##### `const_fold`

The half of the loop's `const_fold` that needs no knowledge of locals: constant
arithmetic and compile-time calls.

##### `branch_prune`

Simplify trivial blocks, and take the constant side of an `if`.

##### `tuple_projection`

Fold `[a, b, c].1` to `b`.

##### `const_ascii_push`

Turn a push of a constant ASCII character into a byte push.

##### `append_fuse`

Reserve capacity once for a run of adjacent string appends.

#### `value_copy_demote`

Make a deep list copy shallow when its elements are never mutated.

#### `sroa_param`

Pass the one field a callee reads instead of the struct, by value or by
reference.

#### `sroa_variant_return`

Return a variant, or hold a variant local, as a `[tag, slots…]` tuple
([WEP: Variant Return Scalarization](./wep-2026-08-03-variant-return-abi.md)).

#### `inline`

Replace a call to a small, non-recursive function with its body. `#[inline]`,
`#[inline(always)]`, and `#[inline(never)]` adjust the decision, and a call on a
cold path is not inlined unless the body is no bigger than the call.
`wado dump --inline-decisions <name>` shows each decision and its reason.

#### `let_block_flatten`

Hoist the statements out of a block a statement evaluates first: a binding's
value, a call's leading argument, a condition. Then the one assignment of a
local no `let` binds becomes its `let`.

#### `sroa`

Split into scalar locals a struct, tuple, or array local used only for element
access, or a local bound to a reference to a struct or tuple literal. A local
handed `&mut` to a call on a cold path splits too. This is the
highest-impact WasmGC pass.

#### `copy_prop`

Propagate a trivial copy and drop the binding.

#### `dae`

Drop a parameter the callee never reads.

#### `drve`

Drop a return value every caller discards.

#### `const_fold`

Partial evaluation: constant arithmetic, compile-time calls, constant globals,
and constant aggregates
([WEP: NIR Interpreter](./wep-2026-04-27-nir-interpreter.md)).

#### `param_spec`

Specialize a callee on the constant arguments its callers pass, and on the
constant fields of a struct they pass by reference.

#### `licm`

Hoist loop-invariant field reads and arithmetic out of a loop. It also drops a
bounds or range check a dominating condition already decides, the in-loop half
of [`cond_impl_post_promote`](#cond_impl_post_promote).

### After the Loop

These run once each, in this order. DCE runs again between `clone_forward` and
`promote_fields`.

#### `field_scalarize`

Keep a hot GC field in a local across a loop.

#### `store_load_forward_post_scalarize`

Forward a stored value to a later load, and fold and prune what that decides.

#### `const_object_globalization`

Share a constant aggregate as an immutable global
([WEP](./wep-2026-05-31-const-object-globalization.md)).

#### `const_fold_post_global`

Fold the reads of the globals `const_object_globalization` made, and prune the
checks they decide.

#### `scalar_forward`

Fold a single-use scalar temp into its use.

#### `clone_forward`

Collapse a clone of a clone into one.

#### `promote_fields`

Fold pure field reads into the value pool, so the checks they bound read as
constants.

#### `cond_impl_post_promote`

Drop a bounds or range check a dominating condition already decides, or the
half of a range check it decides. A later check on the same path, with nothing
observable between, moves into an earlier one: `a[i]; a[i + 1]; a[i + 2]`
checks `i < len - 2` once.

#### `loop_version_bce`

Split a loop into a check-free fast path and the original, and turn a fill loop
into `array.fill`.

#### `unsigned_range_check`

Fuse a remaining `0 <= i && i < n` into one unsigned compare.

#### `select_lowering`

Lower an `if` whose arms are pure and cannot trap to `select`.

#### `split_and_condition`

Split `if a && b { … }` with no `else` into `if a { if b { … } }`, so the
branch on `a` is not tested twice.

#### `multi_value_return`

Return a tuple or struct as one Wasm result per field.

#### `multi_value_param`

Pass an aggregate read only by field as one parameter per field it reads.

#### `freeze_pure_arith`

Fold the remaining pure arithmetic into the value pool, for WIR to emit as
operands.

## WIR Passes

A WIR pass stays only if it changes the emitted Wasm. One that NIR or another
WIR pass already covers is removed. At every level, nullable references are
lowered first. `-O0` then only removes dead items and settles the locals.

Two steps run between the passes below without a name of their own. DCE removes
the functions, globals and types left unreferenced, after `infer_branch_hints`.
The locals are settled last: a non-null local that Wasm's block-scoped rule
sees read before any write is declared nullable, and the locals are grouped by
Wasm type, the most used first.

A `#![wasm_module(...)]` core module, such as the allocator, runs the same list
on its own, with its passes named `wir/<module>:<pass>`.

### `propagate_trivial_copies`

Propagate a copy between two locals.

### `elide_adjacent_box_locals`

Replace a box local read once, right after it is built, with its value.

### `unwrap_box_locals`

Retype a box local to the value it holds.

### `forward_struct_field_constants`

Forward a struct field known to be constant, so a bounds check with a constant
index folds.

### `promote_constant_arrays_to_data`

Build an array of constants from a data segment where that is smaller.

### `split_large_array_literals`

Build a large array literal by filling a fresh array.

### `elide_zero_fill_of_fresh_arrays`

Drop the zero fill of a fresh array that is written before it is read.

### `reuse_struct_loads`

A repeated field load reuses the first, or the value last stored to the field.

### `elide_overwritten_stores`

Drop a field store the next store to the same field replaces before any read.

### `run_peephole`

Instruction selection: `select`, rotates, fused sign extension, and redundant
casts and null checks dropped.

### `flatten_seq_assignments`

Turn an assignment of a statement sequence into the sequence and a plain
assignment.

### `propagate_trivial_copies_post_sroa`

[`propagate_trivial_copies`](#propagate_trivial_copies) over the copies the
pass above exposes.

### `flatten_variant_slots`

Split a variant result slot that holds another small variant.

### `unbox_handles`

Keep a resource as its bare handle where no reference reaches its box.

### `elide_dead_stores`

Drop a store to a local that every path overwrites, or leaves the function
after, before reading it.

### `promote_const_global_inits`

Give a global whose initializer became constant a Wasm constant initializer.

### `cleanup_global_inits`

Normalize global initializers to the form function bodies take.

### `share_unit_cases`

Build each payload-less case of a boxed variant once, as a global.

### `dedupe_const_globals`

Merge identical immutable constant globals.

### `cleanup`

Remove dead locals, no-ops, and dead code after a trap.

### `prune_dead_data`

Remove a data segment nothing reads.

### `select_br_if`

Turn `if c { br L }` into `br_if`.

### `elide_empty_work`

Remove a call to a function that does nothing, and the once-flag that guarded
it.

### `unroll_array_loops`

Give a small loop that reads GC arrays a copy running several iterations per
test of its condition. The original loop runs what is left. Not at `-Os`.

### `infer_branch_hints`

Hint a branch whose one side always traps as unlikely to take it.

### `forward_once_read_locals`

Forward a local read once into its read.

### `coalesce_locals`

Locals of one Wasm type that are never live at once share a slot, and so do the
two ends of a copy.

### `fuse_remaining_local_tees`

Turn a `local.set` and the read right after it into a `local.tee`.

### `elide_fused_write_only_locals`

Remove a local the tee fusion left unread.

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
branch hints. The hints read the markers that say a branch is unlikely; the
other passes read the ones that say code is cold. `cold_path()` says both.

### Hints and Flags

A branch whose one side is cold gets a hint that the other side is likely.
wasmtime lays out the cold side out of line. A branch to a trap is hinted the
same way, marker or not.

`-f no-branch-hinting` leaves the hints out of the Wasm, so a benchmark measures
what they are worth to the engine. `-f no-cold-paths` drops every marker, the
compiler's included, before any pass reads one, so a benchmark measures what the
markers are worth to the whole pipeline.

## Profile-Guided Optimization

The rules are in [the specification](./spec-pgo.md), and the design in
[WEP: Profile-Guided Optimization](./wep-2026-10-25-profile-guided-optimization.md).

Applying a profile marks each region by the two halves of rarity
(`crate::rarity`). An unlikely and cold region gets `cold_path()`. An unlikely
one alone gets `unlikely_path()`, which only the branch hints read. A cold one
alone gets `cold_code()`, which every other pass reads and the hints do not.
`-f no-pgo` ignores the profile, and `-f no-cold-paths` ignores every marker,
so three builds of one program can be compared.

Two questions stay open while profiles spread, and each optimizer change that
touches cold paths answers them again:

- What a profile gains: `benchmark/` compared across the three builds.
- What cold code costs the compile: every pass still runs on a cold function,
  so part of the compile goes to code that rarely runs. The span trace in the
  `profiling-wado-compiler` skill measures the share.

## Differential Testing (EMI)

`wado-compiler/tests/emi.rs` injects code behind a guard that is always false at
run time, and compiles programs under random profiles. It checks that the
program's output does not change at any level. `mise run emi-calibrate`,
`emi-mutate` and `emi-profile` run it, and CI runs it nightly.
See [WEP: Compiler Fuzzing](./wep-2026-08-19-compiler-fuzzing.md).

## Not Yet Implemented

Architectural work is tracked in
[WEP: NIR Optimizer Architecture](./wep-2026-06-05-nir-optimizer-architecture.md).

- [ ] Sparse conditional constant propagation, intra- and interprocedural.
- [ ] Global value numbering across effectful nodes.
- [ ] Instruction combining over identical operands (`x - x`, `x & x`, `~(~x)`).
- [ ] Dead store elimination at NIR, and of field and array stores.
- [ ] Deleting a loop that computes nothing a later read sees, such as one that
  only drains an iterator: `for let b of s.bytes() {}` still walks every byte.
- [ ] Removing a struct field only written through a reference to it: a field
  passed as `&mut s.f` counts as read, even to a callee that only writes it.
- [ ] Strength reduction, reassociation, jump threading, and CFG simplification.
- [ ] Cross-block copy propagation.
- [ ] Sinking a pure definition into the branch that uses it.
- [ ] Forwarding a local bound to a global read.
- [ ] Devirtualizing effect dispatch.
- [ ] Specializing in `param_spec` only where a constant decides a branch.
- [ ] Compile-time evaluation through a `&mut` held in a struct field.
- [ ] Argument promotion: passing a by-reference parameter's fields by value.
- [ ] Factoring a conjunctive `if` chain into a decision tree.
- [ ] Scalarizing a struct referenced by `&mut` off a cold path.
- [ ] Tail calls (`return_call`).
- [ ] Folding an effect-free call on constants whose callee exceeds the inline
  budget.
- [ ] Removing an unused call by its callee's declared effects, without
  inlining it first.
- [ ] Pricing a splice by what its call site keeps: one arm of a dispatch
  every site decides (`$hole_fmt`), or one element of a returned tuple
  (`default_slot`).
- [ ] An array literal's length as a known constant.
- [ ] Dropping the code pointer from a closure no call reaches indirectly.
- [ ] Merging functions that differ only in a constant, a type index or a
  callee, such as the `next_field<T>` of each derived `Deserialize` or the
  clones `param_spec` makes on one constant.

## Tried and Found Ineffective

- An empty-array singleton for default `String` fields: no measurable gain.

## References

- LICM: [CSC D70 LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf), [Cornell CS 6120 loop reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/).
- LLVM: [analysis & transform passes](https://llvm.org/docs/Passes.html), [how LLVM optimizes a function](https://blog.regehr.org/archives/1603), [frontend performance tips](https://llvm.org/docs/Frontend/PerformanceTips.html).
- WasmGC: [Wasm 3.0](https://webassembly.org/news/2025-09-17-wasm-3.0/), [GC proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md), [V8 WasmGC porting (incl. escape analysis)](https://v8.dev/blog/wasm-gc-porting), [Binaryen optimizer cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook).
- SROA: [scalar replacement of aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form).

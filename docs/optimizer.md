# Wado Optimizer

This document describes the optimization passes implemented in the Wado compiler.

## Philosophy: Leverage WebAssembly Native Instructions

When WebAssembly provides native instructions for a feature, use them directly rather than implementing complex compiler transformations. This reduces compiler complexity, leverages runtime JIT optimizations, and produces smaller output.

Examples: `select` for branchless conditionals, `array.copy`/`array.fill` for bulk operations, `return_call` for tail calls (not yet implemented).

## Optimization Levels

All levels run DCE (Dead Code Elimination) to remove unreachable functions, types, and globals.

| Flag            | DCE | Iterations | Inline Threshold |
| --------------- | --- | ---------- | ---------------- |
| `-O0`           | Yes | 0          | N/A              |
| `-O1`           | Yes | 2          | 5                |
| `-O2` (default) | Yes | 10         | 12               |
| `-O3`           | Yes | 100        | 20               |
| `-Os`           | Yes | 10         | 12               |

`-Os` additionally strips the Wasm name section. Optimization passes run in a fixed-point loop with early exit on convergence.

## Optimization Pipeline

The optimizer runs after lowering and before Wasm emission:

1. **Early DCE**: remove unreachable functions/types/globals (all levels)
2. **Fixed-point iteration loop** (skipped for `-O0`):
   1. Container SROA (`container_sroa.rs`)
   2. Function Inlining (`inline.rs`)
   3. LabeledBlock Fusion (`labeled_block_fusion.rs`)
   4. Reference Elimination (`ref_elim.rs`)
   5. SROA (`sroa.rs`)
   6. Copy Propagation (`copy_prop.rs`)
   7. Common Subexpression Elimination (`cse.rs`)
   8. Store-to-Load Forwarding (`store_load_forward.rs`)
   9. Constant Propagation (`const_propagation.rs`)
   10. Constant Folding (`const_folding.rs`)
   11. Constant Global Promotion (`const_global_promotion.rs`)
   12. Constant Branch Pruning (`const_branch_prune.rs`)
   13. Loop-Invariant Code Motion (`licm.rs`)
   14. Condition Implication (`condition_implication.rs`)
   15. Template String Buffer Hoisting (`tmpl_hoist.rs`)
3. **Hot Field Scalarization** (`field_scalarize.rs`): runs once after the loop converges
4. **Final DCE**: clean up code made dead by optimizations (all levels)
5. **Select Lowering** (`select_lowering.rs`): post-optimization rewrite (all levels)
6. **WIR-level optimizations** (`wir_optimize.rs`): see [WIR Optimizations](#wir-optimizations)

Source: `optimize.rs` orchestrates steps 1–5. `wir_optimize.rs` handles step 6.

## TIR Optimization Passes

All TIR passes live in `wado-compiler/src/optimize/`.

### Function Inlining (`inline.rs`)

Replaces small pure function calls with their body. Uses expression-count-based threshold for size estimation.

Eligibility: pure (no effects), non-recursive, no reference parameters/returns, no generics, not from core library, expression count below threshold.

Inline hints via `#[inline]` attributes:

- `#[inline]` — 5x threshold multiplier
- `#[inline(always)]` — always inline regardless of size
- `#[inline(never)]` — never inline

### LabeledBlock Fusion (`labeled_block_fusion.rs`)

Eliminates intermediate GC variant allocations that survive function inlining. When an inlined function returns `Option<T>`, TIR generates:

```
let __tmp = label: { ...break label: null / break label: Some(v)... };
if VariantTest(__tmp, Some) { ... } else { ... }
```

The pass merges this into a single labeled block, routing `break null` to the else branch and `break Some(v)` to the then branch. The intermediate variant allocation is eliminated entirely.

### Reference Elimination (`ref_elim.rs`)

Eliminates unnecessary reference bindings introduced during inlining. When `let self: &T = &local_var` is followed by field accesses only, replaces them with direct field access on the original variable.

### Container SROA (`container_sroa.rs`)

Decomposes `Array<Tuple<T1, ..., Tn>>` and `Array<UserStruct>` local variables into N parallel `Array<T_k>` locals (Array-of-Structs → Struct-of-Arrays). Eliminates the per-element `struct.new` allocation for the container payload in container-of-tuple / container-of-struct idioms such as the zlib Huffman-tree insertion sort.

Motivation: before this pass, writing `let items: Array<[i32, i32]> = []; items.push([a, b]);` forced a per-element `struct.new "tuple//[i32, i32]"` allocation on every push, even though the tuple is immediately consumed by `push` and never escapes. The surrounding `Array<Tuple<T>>` local looks opaque to the ordinary `sroa.rs` pass because the tuple lives on the GC heap behind the array, not as an addressable local. Hoisting the decomposition to the array level eliminates the tuple allocation altogether and exposes the underlying integers for downstream passes. Tuples and user structs are both WasmGC structs at the Wasm level, so the pass treats them uniformly — an `Array<Point>` with `struct Point { x: i32, y: i32 }` is decomposed into `Array<i32>` / `Array<i32>` locals just like `Array<[i32, i32]>`.

Whitelist-based escape analysis — a candidate local is only decomposed when every use matches one of the following patterns:

- `v.push(src)` where `src` is either a matching literal (`[e0, ..., ek]` for tuple layout, `StructName { ... }` for struct layout) or another candidate's `other.index_value(j)` with a compatible layout
- `v[i] = src` (same constraint on the right-hand side; the index must be `is_duplicable_expr` because the rewrite clones it N times)
- `v[i].K` / `v[i].name` (field access on an index result — constant `K` for tuples, field name for structs)
- `v.len()` / `v.is_empty()`
- Initialization via `let v: Array<Elem> = []` or `Array::<Elem>::with_capacity(n)`

Any other use (bare local reference, `&v` / `&mut v`, closure capture, unrecognized method) marks the local as escaped. Source dependencies propagate via a fixpoint: if `b.push(a[j])` is used and `a` escapes, `b` escapes too. Cross-candidate `index_value(j)` sources additionally require `j` to be `is_duplicable_expr` — the rewrite clones the index once per field, so a function-call index forces the candidate to escape.

When a candidate is decomposed, the pass allocates N new `Array<T_k>` locals, rewrites each `push`/`index_assign` into N parallel calls on the per-field arrays, and redirects `v[i].K` reads and `v.len()`/`v.is_empty()` queries to the k-th (or 0-th) field array. For struct candidates, source literals are reordered by `field_index` so that output position k always corresponds to the k-th declared field, independent of the source-level field-assignment order. Requires monomorphized `Array<T_k>::{with_capacity,push,len,IndexValue,IndexAssign}` methods to be present in the catalog; candidates missing any required method are dropped.

The pass runs first in the fixed-point loop so the whitelist sees unobfuscated method-call patterns before `inline.rs` rewrites them. Method call receivers in lowered TIR are `Unary::{Ref,MutRef}` wrapping a `Local`, so `receiver_local()` sees through those wrappers.

Future directions (not yet implemented):

- **Nested containers**: `Array<Array<T>>` is still a hard escape today. Recognising known inner shapes (e.g. fixed-width vectors) would let the outer array be decomposed.
- **Container fields of structs**: today only top-level `let` bindings are candidates. Extending HFS to hoist an `Array<Tuple<...>>` struct field into a local first would give this pass a chance to run on it.
- **Push-to-tuple-literal fusion with `array.new_fixed`**: when the full set of pushes on a decomposed local is statically known (e.g. init lists), emit `array.new_fixed` for each field array instead of a sequence of `append` calls. Pairs well with the existing WIR-level `collapse array append sequences` phase.
- **Parallel index-assign coalescing**: adjacent field-level stores (`v_0[i] = ...; v_1[i] = ...;`) duplicate the bounds check and dispatch. A post-pass that shares the index and the bounds-checked array reference between the N stores would recover the remaining overhead versus hand-written SoA code.
- **Cross-function propagation**: a candidate escapes the moment it is passed to a non-inlined function. Adding a `stores`-aware summary of how callees use their `&mut Array<Tuple<...>>` parameters would extend the pass across function boundaries without losing soundness.

### Scalar Replacement of Aggregates (`sroa.rs`)

Decomposes struct and tuple allocations into individual scalar locals, eliminating GC heap allocations. This is the single most impactful optimization for WasmGC-targeting compilers.

Two-tier escape analysis:

- **Safe (non-escaping):** Only field reads/writes and Move wrappers. Fully decomposed.
- **Soft escape (reconstructible):** Escapes only to call arguments, return statements, or labeled block breaks. Decomposed with reconstruction at escape sites.
- **Hard escape (excluded):** Address taken, captured by closure, or stored into another aggregate. Not decomposed.

### Copy Propagation (`copy_prop.rs`)

Eliminates trivial copy bindings (`let x = y`, `let x = 42`, `let x = true`) by propagating the source value to all uses, then removing the dead binding.

### Store-to-Load Forwarding (`store_load_forward.rs`)

When a literal value is stored to a local variable and then loaded with no intervening modification, forwards the stored value directly. Uses selective invalidation at control flow boundaries — only locals actually modified within branches are invalidated.

### Constant Propagation (`const_propagation.rs`)

Replaces `GlobalVarGet` references to immutable global variables with their constant values.

### Constant Folding (`const_folding.rs`)

Evaluates compile-time-known expressions into literal values: integer/float arithmetic and comparison, boolean logic, bitwise operations, integer casts. Guards against division by zero and signed `MIN / -1` traps. Also folds boolean identity expressions: `false || x` → `x`, `true && x` → `x`, `x || false` → `x`, `x && true` → `x`.

### Constant Global Promotion (`const_global_promotion.rs`)

After constant propagation and folding reduce runtime initializations to scalar constants, promotes those globals back to immutable compile-time constants. Enables cascading optimization with constant propagation in subsequent iterations.

### Common Subexpression Elimination (`cse.rs`)

Eliminates duplicate pure binary expressions within loop bodies. When the same expression appears in the loop guard and in the loop body, and the operand locals are not modified between occurrences, the expression is computed once into a local and reused. Targets patterns left by idiomatic loops like `while p * p <= limit { ... multiple = p * p; ... }`.

### Constant Branch Pruning (`const_branch_prune.rs`)

Eliminates branches with compile-time-known boolean conditions. Also simplifies degenerate block patterns (single-expression blocks, trivial labeled blocks, empty blocks).

Additionally performs labeled block copy propagation: when a labeled block expression starts with `let x = y` (immutable copy from a local), and neither `x` nor `y` is modified within the block, substitutes `x` with `y` and removes the binding. This eliminates residual parameter copies left by function inlining. Combined with the existing `label: { break label: val; }` simplification, this flattens trivial labeled blocks entirely.

### Loop-Invariant Code Motion (`licm.rs`)

Hoists loop-invariant field accesses out of loops when the target variable does not change within the loop body.

### Condition Implication (`condition_implication.rs`)

Eliminates conditions implied false by dominating guards. When a loop guard proves `i < bound`, any inner condition `i >= bound` is known false and can be replaced with `false`. The existing `const_branch_prune` pass then removes the dead branch on the next iteration.

Also handles dominating if-conditions: when `if (var + offset) < bound { ... }`, bounds checks `(var + k) >= bound` for `k <= offset` inside the then-block are known false.

Short-circuit `||` elimination: in `(var + k) >= bound || expr`, the right operand only executes when `var + k < bound`, so any inner `if (index >= bound) { panic }` inside `expr` is always false.

Early-exit guard propagation: when `if (var >= bound) { return/break; }` is followed by subsequent statements, those statements know `var < bound`, eliminating redundant bounds checks below the guard.

This subsumes the former WIR-level bounds check elimination pass, handling both strict `<` and inclusive `<=` guard patterns at the TIR level.

### Template String Buffer Hoisting (`tmpl_hoist.rs`)

Hoists the backing array allocation for template strings out of loops. Each iteration reuses the same backing array buffer. Escape analysis ensures the template result is not stored beyond the iteration.

### Hot Field Scalarization (`field_scalarize.rs`)

Hoists frequently accessed struct fields from GC heap objects to local scalar variables for the duration of a loop. Runs once after the fixed-point loop converges to avoid spurious re-triggering from the write-back/re-read statements it inserts.

Write-back statements are inserted before `return` and `break` statements that exit the HFS loop scope. An optimization sinks write-backs for unlabeled `break` at loop depth 0, since those exit the HFS loop directly and the post-loop write-backs already cover them. This is tracked via a `loop_depth` counter that increments when recursing into nested loops.

### Dead Code Elimination (`dce.rs`)

Removes unreachable functions, types, unused string literals, and unused WASI effect/function imports via call graph reachability analysis from the entry point. Also tracks feature usage (Stdout, Stderr, canonical builtins, box primitives) for conditional feature inclusion.

### Select Lowering (`select_lowering.rs`)

Post-optimization rewrite that converts `if cond { a } else { b }` where both branches are pure into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction.

### TIR Visitor (`tir_visitor.rs`)

Shared visitor infrastructure (`TirMutVisitor` / `TirRefVisitor` / `TirOptVisitor`) for traversing and rewriting TIR trees. `TirOptVisitor` provides change-tracking (`-> bool`) used by optimization passes; the `opt_walk_*` free functions implement the default recursive walk. Utility functions like `block_has_break_to` are also provided here.

### WIR Visitor (`wir_visitor.rs`)

Shared visitor infrastructure (`WirRefVisitor` / `WirMutVisitor`) for traversing and rewriting WIR instruction trees. Centralizes Block/Loop/If/Seq body traversal so individual optimization passes don't duplicate walk logic. Passes that only need body-level traversal (not expression children) override `visit_instr` to skip the `for_each_child` fallback.

## WIR Optimizations

**Module:** `wir_optimize.rs`

WIR-level optimizations run after WIR build and before Wasm emission, operating on the `WirPackage` in-place. They are organized into phases:

### Phase 1: Type Representation

- **Nullable ref optimization** — rewrites type-level representations for nullable references
- **Pre-SROA copy propagation** — inlines trivial copies like `alias = source` so that SROA can see direct variant access patterns (RefTest/RefCast on source)
- **Multi-value return SROA** — rewrites functions returning small scalar structs (2–4 fields) to use Wasm multi-value returns, eliminating GC struct allocation at function boundaries
- **Single-field parameter SROA** — rewrites `ref null S` parameters (where `S` is a single-field struct) to take the scalar field value directly. Primary trigger is `Box<T>` from template string interpolation.

### Phase 2: Single-Field Struct Local Elimination (Round 1)

After parameter SROA, substitutes `StructGet(LocalGet(x), field)` with the inner value when `x` is defined by `LocalSet(x, StructNew { [inner] })`.

### Phase 3: Data Flow

- **Collapse array append sequences** — merges consecutive `append` calls into `array.new_fixed`
- **Forward struct field constants** — tracks known field values (constants and `LocalGet` references) through `StructGet` for constant index bounds check elimination. Also resolves block-result `StructNew` patterns for single-exit blocks. Uses stores-aware alias analysis: locals passed to functions without `stores` declarations are not marked as aliased, enabling field forwarding even when references are passed to callees

### Phase 4: Library-Specific Rewrites

- **Simplify short string appends** — rewrites `append(short_const)` to `append_char` sequences
- **Constant array data promotion** — replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type (≥16 elements)
- **Split large array literals** — rewrites `array.new_fixed` with >256 elements into `array.new_default` + `array.set` to avoid pathological JIT register allocation

### Phase 5: Peephole and Multi-Field Struct Elimination

- **Peephole** — constant folding, copy elision (including cross-scope fresh value tracing for unwrap patterns, StructNew/ArrayNewFixed field usage, and Return/Br instructions containing StructNew with the variable as a field), multi-value struct elision at WIR level
- **Flatten seq assignments** — exposes multi-field struct locals for elimination
- **Multi-field struct local elimination** — substitutes `StructGet(LocalGet(x), field_k)` with the corresponding field expression when all fields are accessed exactly once

### Phase 6: Dead Value Elimination

- **Dead argument elimination (DAE)** — removes unused function parameters and corresponding arguments at call sites when all dead-position arguments are side-effect-free
- **Dead return value elimination (DRVE)** — converts functions whose return value is always dropped to void return
- **Write-only local elimination** — removes `LocalSet(x, expr)` when local `x` is never read. Runs iteratively.

### Phase 7: Global Cleanup

- **Trivial init-guard removal** — removes compiler-generated module-initialization guard blocks when no actual initialization work remains

### Phase 8: Final DCE and Compaction

- **Dead type elimination** — removes GC type definitions not referenced by any live code (transitive)
- **Compact dead items** — removes all items marked dead from the module

## Not Yet Implemented

- **Sparse Conditional Constant Propagation (SCCP)** — simultaneous constant propagation and dead branch elimination
- **Interprocedural SCCP (IPSCCP)** — extends SCCP across function boundaries
- **Global Value Numbering** — generalized CSE with hash-consing (basic loop-level CSE is implemented in `cse.rs`)
- **Peephole / Instruction Combining** — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`, etc.)
- **Dead Store Elimination**
- **Strength Reduction** — loop induction variable optimization
- **Cross-block Copy Propagation**
- **Return Scalarization via Multi-Value Returns** — for user-defined functions (already done for builtins)
- **Function Specialization** — specialize for known constant arguments
- **Argument Promotion** — promote `&T` fields to scalar parameters
- **Jump Threading**
- **Reassociation** — reorder associative operations to group constants
- **SimplifyCFG** — general control flow graph simplification
- **Tail Call Optimization** — emit `return_call` for tail-recursive calls
- **Bounds Check Elimination (chained sequential access)** — `arr[0]; arr[1]; arr[2]` where a single length check could guard all accesses

## Tried and Found Ineffective

- **Empty array singleton for struct field defaults** — sharing a single `array.new<u8>(0)` global across all default String initializations in serde `Deserialize` impls. Measured no performance improvement; the GC allocator handles tiny zero-length arrays efficiently enough that the overhead is negligible compared to other costs.
- **`array.copy` for Array grow** — replacing the element-by-element copy loop in `Array::grow` with the Wasm `array.copy` instruction. Was several times _slower_ than the loop implementation, likely due to poor JIT optimization of `array.copy` in current runtimes.

## Testing Strategy

1. **Golden Fixtures:** `tests/fixtures.golden/*.wir.wado` captures optimized WIR output. Regenerate with `mise run update-golden-fixtures`.
2. **Benchmark Suite:** sieve, mandelbrot, count-prime benchmarks.
3. **Correctness Tests:** E2E fixtures ensure optimizations preserve semantics.
4. **WIR pattern tests:** `wir_expect:Ox` / `wir_not_expect:Ox` in E2E fixtures verify specific optimization effects.

## References

### Loop Optimizations

- [CSC D70: Compiler Optimization LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf)
- [Cornell CS 6120: Loop Reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/)

### LLVM Optimizations

- [LLVM's Analysis and Transform Passes](https://llvm.org/docs/Passes.html)
- [How LLVM Optimizes a Function](https://blog.regehr.org/archives/1603)
- [Performance Tips for Frontend Authors](https://llvm.org/docs/Frontend/PerformanceTips.html)

### Bounds Check Elimination

- [Array Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)

### WebAssembly

- [Wasm 3.0 Release (September 17, 2025)](https://webassembly.org/news/2025-09-17-wasm-3.0/)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [V8: WasmGC Porting](https://v8.dev/blog/wasm-gc-porting)
- [Binaryen Optimizer Cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook)

### Escape Analysis

- [V8: WasmGC Porting — Escape Analysis](https://v8.dev/blog/wasm-gc-porting)
- [Scalar Replacement of Aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form)

### General Compiler Optimization

- [Optimizing Compiler (Wikipedia)](https://en.wikipedia.org/wiki/Optimizing_compiler)
- [Can You Trust a Compiler to Optimize?](https://matklad.github.io/2023/04/09/can-you-trust-a-compiler-to-optimize-your-code.html)

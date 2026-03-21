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
   1. Function Inlining (`inline.rs`)
   2. LabeledBlock Fusion (`labeled_block_fusion.rs`)
   3. Reference Elimination (`ref_elim.rs`)
   4. SROA (`sroa.rs`)
   5. Copy Propagation (`copy_prop.rs`)
   6. Store-to-Load Forwarding (`store_load_forward.rs`)
   7. Constant Propagation (`const_propagation.rs`)
   8. Constant Folding (`const_folding.rs`)
   9. Constant Global Promotion (`const_global_promotion.rs`)
   10. Constant Branch Pruning (`const_branch_prune.rs`)
   11. Loop-Invariant Code Motion (`licm.rs`)
   12. Template String Buffer Hoisting (`tmpl_hoist.rs`)
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

Evaluates compile-time-known expressions into literal values: integer/float arithmetic and comparison, boolean logic, bitwise operations, integer casts. Guards against division by zero and signed `MIN / -1` traps.

### Constant Global Promotion (`const_global_promotion.rs`)

After constant propagation and folding reduce runtime initializations to scalar constants, promotes those globals back to immutable compile-time constants. Enables cascading optimization with constant propagation in subsequent iterations.

### Constant Branch Pruning (`const_branch_prune.rs`)

Eliminates branches with compile-time-known boolean conditions. Also simplifies degenerate block patterns (single-expression blocks, trivial labeled blocks, empty blocks).

### Loop-Invariant Code Motion (`licm.rs`)

Hoists loop-invariant field accesses out of loops when the target variable does not change within the loop body.

### Template String Buffer Hoisting (`tmpl_hoist.rs`)

Hoists the backing array allocation for template strings out of loops. Each iteration reuses the same backing array buffer. Escape analysis ensures the template result is not stored beyond the iteration.

### Hot Field Scalarization (`field_scalarize.rs`)

Hoists frequently accessed struct fields from GC heap objects to local scalar variables for the duration of a loop. Runs once after the fixed-point loop converges to avoid spurious re-triggering from the write-back/re-read statements it inserts.

### Dead Code Elimination (`dce.rs`)

Removes unreachable functions, types, unused string literals, and unused WASI effect/function imports via call graph reachability analysis from the entry point. Also tracks feature usage (Stdout, Stderr, canonical builtins, box primitives) for conditional feature inclusion.

### Select Lowering (`select_lowering.rs`)

Post-optimization rewrite that converts `if cond { a } else { b }` where both branches are pure into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction.

### TIR Visitor (`visitor.rs`)

Shared visitor infrastructure used by optimization passes to traverse and rewrite TIR trees.

## WIR Optimizations

**Module:** `wir_optimize.rs`

WIR-level optimizations run after WIR build and before Wasm emission, operating on the `WirModule` in-place. They are organized into phases:

### Phase 1: Type Representation

- **Nullable ref optimization** — rewrites type-level representations for nullable references
- **Multi-value return SROA** — rewrites functions returning small scalar structs (2–4 fields) to use Wasm multi-value returns, eliminating GC struct allocation at function boundaries
- **Single-field parameter SROA** — rewrites `ref null S` parameters (where `S` is a single-field struct) to take the scalar field value directly. Primary trigger is `Box<T>` from template string interpolation.

### Phase 2: Single-Field Struct Local Elimination (Round 1)

After parameter SROA, substitutes `StructGet(LocalGet(x), field)` with the inner value when `x` is defined by `LocalSet(x, StructNew { [inner] })`.

### Phase 3: Data Flow

- **Collapse array append sequences** — merges consecutive `append` calls into `array.new_fixed`
- **Forward struct field constants** — tracks known field values (constants and `LocalGet` references) through `StructGet` for bounds check elimination. Also resolves block-result `StructNew` patterns for single-exit blocks. Uses stores-aware alias analysis: locals passed to functions without `stores` declarations are not marked as aliased, enabling field forwarding even when references are passed to callees
- **Eliminate loop-guarded bounds checks** — removes redundant `index >= bound` checks when the loop guard dominates them. Supports both `i < bound` (strict) and `i <= limit` (inclusive) guards. For inclusive guards, resolves definition chains to verify that the bounds check bound equals `limit + 1` (e.g., `arr.used == limit + 1` when `arr = Array::filled(limit + 1, ...)`)

### Phase 4: Library-Specific Rewrites

- **Simplify short string appends** — rewrites `append(short_const)` to `append_char` sequences
- **Constant array data promotion** — replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type (≥16 elements)
- **Split large array literals** — rewrites `array.new_fixed` with >256 elements into `array.new_default` + `array.set` to avoid pathological JIT register allocation

### Phase 5: Peephole and Multi-Field Struct Elimination

- **Peephole** — constant folding, copy elision (including cross-scope fresh value tracing for unwrap patterns), multi-value struct elision at WIR level
- **Flatten seq assignments** — exposes multi-field struct locals for elimination
- **Multi-field struct local elimination** — substitutes `StructGet(LocalGet(x), field_k)` with the corresponding field expression when all fields are accessed exactly once

### Phase 6: Dead Value Elimination

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
- **Common Subexpression Elimination / Global Value Numbering**
- **Peephole / Instruction Combining** — algebraic simplification (`x + 0 → x`, `x * 2 → x << 1`, etc.)
- **Dead Store Elimination**
- **Strength Reduction** — loop induction variable optimization
- **Cross-block Copy Propagation**
- **Return Scalarization via Multi-Value Returns** — for user-defined functions (already done for builtins)
- **Function Specialization** — specialize for known constant arguments
- **Argument Promotion** — promote `&T` fields to scalar parameters
- **Dead Argument Elimination**
- **Jump Threading**
- **Reassociation** — reorder associative operations to group constants
- **SimplifyCFG** — general control flow graph simplification
- **Tail Call Optimization** — emit `return_call` for tail-recursive calls
- **Bounds Check Elimination (outside loops)** — redundant consecutive checks

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

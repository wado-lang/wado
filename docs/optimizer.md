# Wado Optimizer

This document describes the optimization features implemented in the Wado compiler and planned optimizations for future development.

## Philosophy: Leverage WebAssembly Native Instructions

When WebAssembly provides native instructions for a feature, use them directly rather than implementing complex compiler transformations.

WebAssembly 3.0 (released September 17, 2025) provides a rich set of native instructions that should be preferred over compiler-based transformations:

- Tail Calls: Use `return_call`/`return_call_indirect` instead of converting to loops
- Bulk Operations: Use `array.copy`, `array.fill`, `memory.copy`, `memory.fill`
- Bit Operations: Use `i32.clz`, `i32.ctz`, `i32.popcnt` for efficient bit manipulation
- Branchless Code: Use `select` instruction for conditional values without branches
- GC Operations: Already using `array.new`, `struct.new`, `array.get`, `array.set`
- SIMD (future): Use `v128` vector instructions for parallel operations

This approach:

- Reduces compiler complexity
- Leverages runtime optimizations (JIT compilers optimize Wasm instructions)
- Produces smaller compiled output
- Maintains better portability across Wasm runtimes

See [WebAssembly-Native Instruction Opportunities](#webassembly-native-instruction-opportunities) section below for details.

## Optimization Levels

All levels run DCE (Dead Code Elimination) to remove unreachable functions and types, which significantly reduces codegen work.

| Flag            | DCE | Iterations | Inline Threshold |
| --------------- | --- | ---------- | ---------------- |
| `-O0`           | Yes | 0          | N/A              |
| `-O1`           | Yes | 2          | 10               |
| `-O2` (default) | Yes | 10         | 10               |
| `-O3`           | Yes | 100        | 20               |
| `-Os`           | Yes | 10         | 10               |

`-Os` additionally strips the Wasm name section. Optimization passes (inlining, ref-elim, etc.) run in a fixed-point loop with early exit on convergence.

## Optimization Pipeline

The optimizer runs after lowering and before Wasm emission:

1. Early DCE: remove unreachable functions/types/globals before optimization (all levels). This significantly reduces the working set for subsequent passes by eliminating stdlib functions and types the program doesn't use.
2. Fixed-point iteration loop (skipped for `-O0`):
   1. Function Inlining
   2. Reference Elimination
   3. SROA (Scalar Replacement of Aggregates)
   4. Copy Propagation
   5. Store-to-Load Forwarding
   6. Constant Propagation
   6. Constant Folding
   7. Constant Global Promotion
   8. Constant Branch Pruning
   9. Loop-Invariant Code Motion (LICM)
   10. Template String Buffer Hoisting
3. Final DCE: clean up code made dead by optimizations (e.g., functions inlined away) (all levels)
4. Post-optimization rewrites (labeled block simplification, select lowering, move insertion; all levels)
5. WIR-level optimizations (multi-value SROA, constant array data promotion, large array splitting; see [WIR Optimizations](#wir-optimizations))

## Implemented Optimizations

### Function Inlining

**Module:** `optimize/inline.rs`

Eliminates function call overhead by replacing small pure function calls with their body. Uses expression-count-based threshold for accurate size estimation.

Eligibility: pure (no effects), non-recursive, no reference parameters/returns, no generics, not from core library, expression count below threshold.

Inline hints via `#[inline]` attributes override the default heuristics:

- `#[inline]` — prefer inlining (5x threshold multiplier)
- `#[inline(always)]` — always inline regardless of size or threshold
- `#[inline(never)]` — never inline (useful for cold error paths or debugging)

### Reference Elimination

**Module:** `optimize/ref_elim.rs`

Eliminates unnecessary reference bindings introduced during inlining. When `let self: &T = &local_var` is followed by field accesses only, replaces them with direct field access on the original variable.

### Scalar Replacement of Aggregates (SROA)

**Module:** `optimize/sroa.rs`

Decomposes struct and tuple allocations into individual scalar locals, eliminating GC heap allocations. After inlining exposes patterns like `let s = Point { x: expr1, y: expr2 }; let a = s.x;`, SROA replaces the struct with per-field scalar locals (`__sroa_s_x`, `__sroa_s_y`). Copy propagation then cleans up the trivial copies. Mutable field writes (`s.x = value`) are transformed into scalar local assignments.

Two-tier escape analysis determines eligibility:

- **Safe (non-escaping):** The aggregate is only used for field reads, field writes, and reads through `Move` wrappers. Fully decomposed with no reconstruction overhead.
- **Soft escape (reconstructible):** The aggregate escapes only to call arguments, return statements, or labeled block breaks. SROA still decomposes the aggregate into scalars for field accesses, and automatically reconstructs a fresh struct/tuple literal at each escape site. This enables SROA even when the aggregate is partially passed around.
- **Hard escape (excluded):** Address taken (`&s`), captured by a closure, stored into another aggregate, or used as a bare local in a non-reconstructible position. Not decomposed.

This is the single most impactful optimization for WasmGC-targeting compilers, as struct allocations are GC-managed heap objects.

### Copy Propagation

**Module:** `optimize/copy_prop.rs`

Eliminates trivial copy bindings (`let x = y`, `let x = 42`, `let x = true`) by propagating the source value to all uses, then removing the dead binding. Checks safety conditions: target not reassigned, not address-taken, not captured by closures, and for value types the source must be dead after the binding.

### Store-to-Load Forwarding

**Module:** `optimize/store_load_forward.rs`

When a literal value is stored to a local variable and then loaded with no intervening modification, forwards the stored value directly, eliminating the load. Particularly effective after SROA decomposes struct fields into scalar locals: the forwarding propagates known literal values through control flow boundaries (such as assert branches) that would otherwise block optimization. Uses selective invalidation at control flow boundaries — only locals actually modified within branches are invalidated, allowing knowledge of unmodified locals to survive. A pre-analysis phase identifies unsafe locals (address-taken or closure-captured) that are excluded from forwarding.

### Constant Propagation

**Module:** `optimize/const_prop.rs`

Replaces `GlobalVarGet` references to immutable global variables with their constant values. When a global is non-mutable and initialized with a scalar constant (int, float, bool, char literal), all reads are replaced with the literal value. After lowering, any global with `mutable == false` is guaranteed to have a constant initializer, so the propagation is always safe.

### Constant Folding

**Module:** `optimize/const_fold.rs`

Evaluates compile-time-known expressions into literal values:

- Integer arithmetic: add, sub, mul, div, mod on i8–i64 and u8–u64
- Integer comparison: eq, ne, lt, le, gt, ge
- Integer bitwise: and, or, xor, shl, shr
- Integer unary: neg, bitnot
- Integer cast: truncation/extension between integer types
- Float arithmetic: add, sub, mul, div on f32 and f64 (skipped when result is NaN)
- Float comparison: eq, ne, lt, le, gt, ge
- Float unary: neg (sign-bit flip, always deterministic)
- Boolean logical: and, or
- Boolean equality: eq, ne
- Boolean unary: not

Guards against division by zero and signed `MIN / -1` traps.

### Constant Branch Pruning

**Module:** `optimize/dce.rs`

Eliminates branches with compile-time-known boolean conditions. When `if true { A } else { B }` or `if false { A } else { B }` is detected, replaces the branch with the taken side. Also simplifies degenerate block patterns:

- `{ expr; }` → `expr` (single-expression block)
- `label: { break label: val; }` → `val` (trivial labeled block from inlining)
- Empty blocks → `()` (unit)

### Constant Global Promotion

**Module:** `optimize/const_global_promotion.rs`

After constant propagation and folding reduce runtime initializations to scalar constants, this pass promotes those globals back to immutable compile-time constants. Scans `__initialize_module` functions for `GlobalVarSet` with constant values targeting promotable globals (user-declared immutable but forced Wasm-mutable by lowering), updates the initializer, marks immutable, and removes the dead `GlobalVarSet` statements. Enables cascading optimization: promoted constants feed back into constant propagation in subsequent iterations.

### Loop-Invariant Code Motion (LICM)

**Module:** `optimize/licm.rs`

Hoists loop-invariant field accesses out of loops. When a field access targets a variable that does not change within the loop body, it is moved before the loop entry.

### Template String Buffer Hoisting

**Module:** `optimize/tmpl_hoist.rs`

Hoists the backing array allocation for template strings out of loops. When a template string (`` `...{expr}...` ``) appears inside a loop and the result is only used as a method receiver (not passed as a function argument where it could be stored), the `array_new` allocation is moved before the loop. Each iteration reuses the same backing array buffer instead of allocating a new one.

Escape analysis ensures correctness: the optimization is only applied when the template result is bound to a local variable that does not escape (i.e., never passed as a non-receiver function argument, never stored in a struct field). This prevents aliasing bugs where a stored String would share its backing array with future iterations.

Benchmark impact: ~28% speedup on the float-to-string benchmark (500K f64 conversions).

### Dead Code Elimination (DCE)

**Module:** `optimize/dce.rs`

Removes unreachable functions from the compiled output via call graph reachability analysis from the entry point. Also eliminates unreachable types, unused string literals, and unused WASI effect/function imports.

### Feature Analysis and Conditional Feature Inclusion

**Module:** `optimize/dce.rs`

Includes only WASI functions, effects, and builtins that are actually used by reachable code. Tracks effect usage (Stdout, Stderr, etc.), WASI function usage, canonical builtins (stream operations, float-to-string, etc.), and box primitive requirements.

### Labeled Block Simplification

**Module:** `optimize/rewrite.rs`

Eliminates trivial `label: { break label: expr; }` patterns produced by function inlining. Replaces them with the inner expression directly.

### Select Lowering (Branchless Conditional)

**Module:** `optimize/rewrite.rs`

Converts simple `if cond { a } else { b }` expressions where both branches are pure (no side effects, no traps) into `builtin::select(cond, a, b)`, which emits the Wasm `select` instruction. Both operands are evaluated eagerly.

### Move Insertion

**Module:** `optimize/rewrite.rs`

Wraps fresh values (literals, call results) in `Move` nodes to avoid unnecessary value copies. Fresh values are owned by the current expression and can be moved directly.

## Not Yet Implemented

### Library Call Optimization

Rewrites calls to known stdlib functions into more efficient instruction sequences when arguments are compile-time constants. Similar to LLVM's `SimplifyLibCalls` pass.

#### `String.append(short_const)` → `append_char` sequence

When `append` is called with a constant string of 4 bytes or fewer, expand into a sequence of `append_char` calls. Each `append_char` is a simple array grow + set, while `append(str)` requires allocating the constant string as a GC array and looping over its bytes.

#### Template string with single interpolation → direct `to_string`

When a template string contains exactly one interpolation with no prefix or suffix (`` `{expr}` ``), replace with `expr.to_string()` directly. Eliminates the StringBuilder GC allocation entirely.

### Strength Reduction

Transform expensive loop operations into cheaper equivalents. For example, replace `p * p` recomputed each iteration with an accumulator updated by addition.

Patterns to detect:

- `counter * constant` in loops replaced with addition-based accumulator
- `x * x` (squaring) in loops maintained as a separate variable
- `base + counter * step` replaced with incremental addition

### Bounds Check Elimination

Remove redundant array bounds checks when the compiler can prove indices are within bounds via value range propagation. For example, when a loop condition ensures `n <= limit` and the array size is `limit + 1`, the bounds check is redundant.

Can provide 10-30% speedup for array-intensive code.

### Common Subexpression Elimination (CSE)

Identify identical subexpressions and replace with a single computation. Hash expressions to find duplicates, replace with a temporary variable.

Could be extended to Global Value Numbering (GVN), which is more powerful: it detects semantically equivalent computations even when syntactically different (e.g., `a + b` and `b + a`). GVN is one of LLVM's most impactful optimization passes.

### Sparse Conditional Constant Propagation (SCCP)

The current constant propagation handles immutable globals with scalar initializers. SCCP is a more powerful variant that simultaneously propagates constants through local variables and eliminates dead branches, handling inter-dependent constant conditions that the current separate passes miss.

### Interprocedural Sparse Conditional Constant Propagation (IPSCCP)

Extends SCCP across function boundaries. Propagates constants from call sites into callee parameters, and propagates constant return values back to callers. When a function is always called with a constant argument, the parameter is replaced with the constant inside the function body, enabling further folding and dead branch elimination. When a function always returns the same constant, all call sites are replaced with that constant.

LLVM's IPSCCP is one of its most effective interprocedural passes. In Wado's context, it is particularly valuable because:

- Monomorphized generic functions often receive constant arguments (e.g., capacity values, flag booleans)
- Stdlib wrapper functions frequently forward constants through several call layers
- Combined with function specialization, it enables aggressive optimization of hot paths without inlining

Depends on SCCP as the intraprocedural foundation.

### Copy Propagation (Cross-Block)

The current copy propagation works within simple cases. Extending it to handle cross-block propagation using reaching definitions would catch more redundant copies.

### Peephole Optimization / Instruction Combining

Pattern-match small instruction sequences and replace with more efficient equivalents:

```
x + 0       → x
x * 1       → x
x * 2       → x << 1
x / 4       → x >> 2  (for unsigned)
x * 0       → 0
x - x       → 0
x & x       → x
x | 0       → x
x ^ x       → 0
!!x         → x
```

LLVM's InstCombine pass (the most general form of this) rewrites instruction sequences connected by data flow into more efficient forms and is one of the most impactful passes in the LLVM pipeline.

### Dead Store Elimination

Remove assignments to variables that are never subsequently read. Requires liveness analysis.

### Algebraic Simplification

Apply algebraic laws to simplify expressions. Often implemented as part of peephole optimization.

### Return Scalarization via Multi-Value Returns

When a function returns a struct or tuple that is immediately destructured at every call site, the return can be scalarized using Wasm multi-value returns. This eliminates the GC struct allocation at the function boundary without requiring inlining.

The compiler already implements this for builtins (e.g., `i64_add128` returns two `i64` values on the Wasm stack via `MultiValueStructNew`/`MultiValueLocalBind` in WIR). Extending this to user-defined functions is a natural next step. Complements SROA: SROA handles the local case, multi-value handles the cross-function case. Most valuable for functions too large to inline.

### Function Specialization

Create specialized versions of functions for known constant arguments. When constant propagation determines that a function is always called with certain constant arguments, a specialized version can be generated that folds those constants, enabling further optimizations inside the specialized body.

LLVM implements this as `FunctionSpecialization`. It is particularly effective when combined with interprocedural constant propagation.

### Argument Promotion

When a function takes a reference parameter (`&T`) but only accesses specific fields, promote those fields to direct scalar parameters (LLVM: `-argpromotion`). Eliminates the GC ref parameter, which in turn enables caller-side SROA on the struct that was only constructed to pass as a reference. Particularly impactful for `&self` methods that only read a few fields.

### Dead Argument Elimination

Remove function parameters that are unused at all call sites (LLVM: `-deadargelim`). Common after monomorphization where generic parameters or feature-specific arguments become dead. Reduces call overhead and enables further interprocedural optimizations.

### Jump Threading

When a branch condition is already determined along a specific incoming edge, thread the jump directly to the target block, bypassing redundant comparisons (LLVM: `-jump-threading`). Effective for `match` expression chains and sequential `if-else if` patterns.

### Reassociation

Reorder associative and commutative operations to group constants together, enabling more constant folding (LLVM: `-reassociate`). For example, `(x + 1) + 2` → `x + (1 + 2)` → `x + 3`.

### SimplifyCFG

General control flow graph simplification: merge redundant blocks, eliminate empty blocks, simplify trivial branches (LLVM: `-simplifycfg`). The current Constant Branch Pruning is a subset of this. Extending it to handle block merging and unreachable block elimination would clean up more patterns exposed by other passes.

### Tail Call Optimization via `return_call`

Emit WebAssembly's `return_call` instruction for tail-recursive function calls instead of a regular `call` + `return`. Part of Wasm 3.0 standard. Eliminates stack overflow for deep recursion.

## Leveraging Bit Manipulation Intrinsics

The compiler exposes `i32_clz` and `i64_clz` in `core/builtin.wado`, mapped directly to Wasm instructions. Additional intrinsics (`ctz`, `popcnt`) should also be exposed. Beyond direct use by programmers, the optimizer can recognize common patterns and rewrite them to use these intrinsics.

### Intrinsics to Expose

| Builtin         | Wasm Instruction | Status      |
| --------------- | ---------------- | ----------- |
| `i32_clz(x)`    | `i32.clz`        | Exposed     |
| `i64_clz(x)`    | `i64.clz`        | Exposed     |
| `i32_ctz(x)`    | `i32.ctz`        | Not exposed |
| `i64_ctz(x)`    | `i64.ctz`        | Not exposed |
| `i32_popcnt(x)` | `i32.popcnt`     | Not exposed |
| `i64_popcnt(x)` | `i64.popcnt`     | Not exposed |

### Pattern Recognition Opportunities

The optimizer could detect common bit manipulation idioms (log2 via division loop, popcount via shift loop, find-lowest-bit via LSB scan) and rewrite them to use single Wasm instructions (`clz`, `ctz`, `popcnt`). These patterns appear frequently in bitmap data structures, hash tables, and compression algorithms. Recognizing them turns O(n) loops into O(1) instructions.

## WebAssembly-Specific Optimizations

These optimizations could be applied at the WAT/Wasm level or delegated to Binaryen's wasm-opt:

### Stack IR Optimizations

Use Binaryen's Stack IR for optimizations tailored to WebAssembly's stack machine.

### Whole-Program Analysis (--gufa)

Binaryen's `wasm-opt --gufa` infers constant values and exact types in a whole-program manner. Particularly helpful for WasmGC — infers exact types for better optimization, devirtualization, and cast elimination.

### Monomorphization

Generic functions are already monomorphized, but each monomorphized variant could be optimized independently. Binaryen's `wasm-opt --monomorphize` can also specialize call sites.

## WebAssembly-Native Instruction Opportunities

### Already Implemented

**GC Array Operations:**

- `array.new`, `array.new_fixed`, `array.get`, `array.set`, `array.len`
- `array.fill` (exposed as `builtin::array_fill`)
- `array.copy` (exposed as `builtin::array_copy`)

**GC Struct Operations:**

- `struct.new`, `struct.get`, `struct.set`

**Branchless Conditional:**

- `select` (exposed as `builtin::select<T>`, auto-inserted by optimizer)

**Bit Manipulation:**

- `i32.clz` (exposed as `builtin::i32_clz`)
- `i64.clz` (exposed as `builtin::i64_clz`)

### Should Expose

#### Tail Call Instructions

- `return_call` — Direct tail call
- `return_call_indirect` — Indirect tail call
- `return_call_ref` — Reference-based tail call

Implementation: Detect `return func(...)` pattern in TIR, emit `return_call` instead of `call` + `return`.

#### Remaining Bit Manipulation

- `i32.ctz` / `i64.ctz` — Count trailing zeros
- `i32.popcnt` / `i64.popcnt` — Population count

#### Bulk Memory Operations

- `memory.fill` — Fill memory region with byte value
- `memory.copy` — Copy memory region (handles overlapping)

### Future: SIMD (v128)

`v128` vector operations for parallel integer/float arithmetic. Process 4x i32 or 2x i64 in parallel. Useful for array operations, numerical computations, and image/audio processing.

### Summary: Prefer Wasm Instructions Over Compiler Transforms

| Optimization Goal    | Do Not            | Do This Instead                                                   |
| -------------------- | ----------------- | ----------------------------------------------------------------- |
| Tail recursion       | Convert to loop   | Emit `return_call`                                                |
| Array initialization | Emit loop         | Use `array.fill`                                                  |
| Array copy           | Emit loop         | Use `array.copy`                                                  |
| Memory copy          | Byte-by-byte loop | Use `memory.copy`                                                 |
| Memory zero          | Byte-by-byte loop | Use `memory.fill`                                                 |
| Conditional value    | Branch + PHI      | Use `select`                                                      |
| Count bits           | Loop with shifts  | Use `popcnt`                                                      |
| Find first bit       | Loop with shifts  | Use `clz` / `ctz`                                                 |
| Loop unrolling       | Replicate code 4x | Don't — increases code size, Wasm runtimes already optimize loops |

## WIR Optimizations

**Module:** `wir_optimize.rs`

WIR-level optimizations run after WIR build and before Wasm emission, operating on the `WirModule` in-place.

### Multi-Value Return SROA

Rewrites internal functions that return small scalar structs (2–4 fields) to use Wasm multi-value returns, eliminating GC struct allocation at function boundaries. A companion tuple elision pass replaces `MultiValueStructNew` + `StructGet` sequences with `MultiValueLocalBind` at call sites.

### Single-Field Parameter SROA

Rewrites functions that take `ref null S` parameters (where `S` is any single-field struct) to take the scalar field value directly, eliminating GC struct allocation at call sites. This generalizes the original Box-specific optimization to all single-field structs.

The primary trigger is `Box<T>`, a single-field GC struct synthesized by the lower phase to wrap primitives for reference semantics (e.g., `Display::fmt` receives `&self` as `ref null Box<T>`). At call sites where `StructNew(S, [val])` is passed, the value is passed directly as a scalar. For call sites that pass an existing struct reference, a `StructGet` extracts the scalar value.

Impacts all code paths that use template string interpolation (`\`{value}\``), which is the primary trigger for`Box<T>`creation, as well as any user-defined single-field structs with`&self` methods.

### Split Large Array Literals

Rewrites `array.new_fixed` with more than 256 elements into `array.new_default` + per-element `array.set`. This prevents pathological JIT compilation time in Cranelift's register allocator, which degrades severely when thousands of values are simultaneously on the operand stack. The rewrite preserves each element expression as-is, so dynamic expressions (variable references, function calls, arithmetic) work correctly.

### Constant Array Data Promotion (`array.new_data`)

Replaces `array.new_fixed` with `array.new_data` when all elements are compile-time constants of a primitive type. Packs constant values into a passive Wasm data segment and initializes the array via a single `array.new_data` instruction, reducing both Wasm binary size and initialization overhead.

Eligibility: all elements must be compile-time constants (`I32Const`, `I64Const`, `F32Const`, `F64Const`), the array element type must be a packable primitive (`i8`–`i64`, `u8`–`u64`, `f32`, `f64`, `bool`, `char`, enum, flags), and the array must have at least 16 elements. Runs before the split pass, so promoted arrays avoid the `array.new_default` + `array.set` fallback.

## Testing Strategy

1. Golden Fixtures: `tests/fixtures.golden/*.lowered.wado` captures optimized TIR output. Regenerate with `make update-golden-fixtures`. CI integrity check verifies golden files are up-to-date.
2. Benchmark Suite: sieve, mandelbrot, count-prime benchmarks.
3. Correctness Tests: E2E fixtures ensure optimizations preserve semantics.
4. Performance Regression: Track benchmark performance over time.

## References

### Loop Optimizations

- [CSC D70: Compiler Optimization LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf)
- [Loop-Invariant Code Motion](https://grokipedia.com/page/Loop-invariant_code_motion)
- [Cornell CS 6120: Loop Reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/)

### LLVM Optimizations

- [LLVM's Analysis and Transform Passes](https://llvm.org/docs/Passes.html)
- [How LLVM Optimizes a Function](https://blog.regehr.org/archives/1603)
- [Performance Tips for Frontend Authors](https://llvm.org/docs/Frontend/PerformanceTips.html)

### Bounds Check Elimination

- [Array Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)
- [Java HotSpot Bounds Check Elimination](https://www.researchgate.net/publication/221302947_Array_bounds_check_elimination_for_the_Java_HotSpot_client_compiler)

### Peephole and Local Optimizations

- [CMU Lecture on Peephole and CSE](https://www.cs.cmu.edu/~janh/courses/411/23/lec/18-peepsub.pdf)
- [Peephole Optimization (Wikipedia)](https://en.wikipedia.org/wiki/Peephole_optimization)

### WebAssembly Instructions and Features

- [Wasm 3.0 Release (September 17, 2025)](https://webassembly.org/news/2025-09-17-wasm-3.0/)
- [WebAssembly Tail Call Proposal](https://github.com/WebAssembly/tail-call/blob/main/proposals/tail-call/Overview.md)
- [V8 WebAssembly Tail Calls](https://v8.dev/blog/wasm-tail-call)
- [WebAssembly Bulk Memory Operations](https://github.com/WebAssembly/bulk-memory-operations/blob/master/proposals/bulk-memory-operations/Overview.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [WebAssembly SIMD](https://github.com/WebAssembly/simd/blob/main/proposals/simd/SIMD.md)

### WebAssembly Optimizations and Tools

- [Binaryen Optimizer Cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook)
- [V8: WasmGC Porting](https://v8.dev/blog/wasm-gc-porting)
- [V8: Speculative WebAssembly Optimizations](https://v8.dev/blog/wasm-speculative-optimizations)

### Escape Analysis

- [V8: WasmGC Porting — Escape Analysis](https://v8.dev/blog/wasm-gc-porting)
- [Scalar Replacement of Aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form)

### General Compiler Optimization

- [Optimizing Compiler (Wikipedia)](https://en.wikipedia.org/wiki/Optimizing_compiler)
- [Can You Trust a Compiler to Optimize?](https://matklad.github.io/2023/04/09/can-you-trust-a-compiler-to-optimize-your-code.html)

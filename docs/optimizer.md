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

The optimizer runs after lowering and before Wasm plan/codegen:

1. Fixed-point iteration loop (skipped for `-O0`):
   1. Function Inlining
   2. Reference Elimination
   3. SROA (Scalar Replacement of Aggregates)
   4. Copy Propagation
   5. Constant Propagation
   6. Constant Folding
   7. Constant Global Promotion
   8. Constant Branch Pruning
   9. Loop-Invariant Code Motion (LICM)
2. DCE Analysis and removal of unreachable functions/types (all levels)
3. Post-optimization rewrites (labeled block simplification, select lowering, move insertion; all levels)

## Implemented Optimizations

### Function Inlining

**Module:** `optimize/inline.rs`

Eliminates function call overhead by replacing small pure function calls with their body. Uses expression-count-based threshold for accurate size estimation.

Eligibility: pure (no effects), non-recursive, no reference parameters/returns, no generics, not from core library, expression count below threshold.

### Reference Elimination

**Module:** `optimize/ref_elim.rs`

Eliminates unnecessary reference bindings introduced during inlining. When `let self: &T = &local_var` is followed by field accesses only, replaces them with direct field access on the original variable.

### Scalar Replacement of Aggregates (SROA)

**Module:** `optimize/sroa.rs`

Decomposes struct and tuple allocations into individual scalar locals when the aggregate does not escape. After inlining exposes patterns like `let s = Point { x: expr1, y: expr2 }; let a = s.x;`, SROA replaces the struct with per-field scalar locals (`__sroa_s_x`, `__sroa_s_y`), eliminating the GC heap allocation. Copy propagation then cleans up the trivial copies.

Escape analysis ensures safety: a candidate is decomposed only if it is never passed to a function, returned, address-taken, captured by a closure, or stored into another aggregate. Field reads, field writes, and reads through `Move` wrappers are allowed.

This is the single most impactful optimization for WasmGC-targeting compilers, as struct allocations are GC-managed heap objects.

### Copy Propagation

**Module:** `optimize/copy_prop.rs`

Eliminates trivial copy bindings (`let x = y`, `let x = 42`, `let x = true`) by propagating the source value to all uses, then removing the dead binding. Checks safety conditions: target not reassigned, not address-taken, not captured by closures, and for value types the source must be dead after the binding.

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

The compiler already implements this for builtins (e.g., `i64_add128` returns two `i64` values on the Wasm stack via `MultiValueStructNew`/`MultiValueLocalBind` in WIR). Extending this to user-defined functions is a natural next step.

#### Current State: Builtin Multi-Value Pipeline

The existing pipeline for builtins:

1. `lower.rs`: `is_multivalue_builtin_pattern()` detects builtin multi-value calls and **preserves** `LetPattern` (instead of lowering to `Let + FieldAccess`)
2. `wir_build/translate.rs`: Wraps the Wasm multi-value instruction in `MultiValueStructNew`
3. `translate_let_pattern()`: Detects `MultiValueStructNew` → replaces with `MultiValueLocalBind` (tuple elision, no struct allocation)

#### Goal: Extend to User-Defined Functions

Example:

```wado
fn minmax(a: i32, b: i32) -> [i32, i32] {
    if a < b { return [a, b]; }
    return [b, a];
}
let [lo, hi] = minmax(x, y);  // immediate destructuring
```

Current codegen:

```wat
(func $minmax (param i32 i32) (result (ref $tuple_i32_i32))
  ;; ... compute ...
  (struct.new $tuple_i32_i32)  ;; heap allocation
)
;; caller:
(call $minmax)
(local.tee $tmp)
(struct.get $tuple_i32_i32 0)  ;; field extraction
(local.set $lo)
(local.get $tmp)
(struct.get $tuple_i32_i32 1)
(local.set $hi)
```

Optimized codegen with multi-value return:

```wat
(func $minmax (param i32 i32) (result i32 i32)
  ;; ... compute ...
  ;; no struct.new — two i32 values on stack
)
;; caller:
(call $minmax)
(local.set $hi)  ;; directly from stack (LIFO order)
(local.set $lo)
```

#### Implementation Phase: Hybrid TIR Analysis + WIR Generation

Two approaches were considered:

**Option A: Pure WIR optimization pass** — Add an optimization phase after WIR build that rewrites signatures and call sites. WIR already has `MultiValueStructNew`/`MultiValueLocalBind` infrastructure and Wasm-level types. However, WIR currently has no optimization framework, and introducing one is a significant architectural change.

**Option B (preferred): Hybrid TIR analysis + WIR generation** — The TIR optimizer identifies candidate functions via whole-program analysis and marks them. WIR build then uses these marks to generate multi-value signatures and call sites.

Rationale for Option B:

1. **Whole-program analysis is natural at TIR level** — The TIR optimizer already performs cross-function analysis (inlining, DCE). Adding call-site analysis fits this model.
2. **Leverages existing fixed-point iteration** — SROA decomposes struct locals, copy propagation cleans up, then multi-value candidate detection runs in the same loop. Interplay between passes is valuable.
3. **No new WIR optimization framework needed** — The "decision" happens at TIR, the "execution" happens during WIR build (which already handles the builtin case).
4. **Clean separation of concerns** — "What to optimize" is a TIR-level question. "How to emit multi-value Wasm" is a WIR-level question.

Note: A WIR optimization framework may be useful in the future for other purposes (stack scheduling, register allocation, peephole). Option B does not preclude adding one later.

#### Implementation Strategy

1. **TIR analysis pass** (new pass in `optimize/`): Scan all internal functions that return a struct or tuple. For each, check that ALL call sites either:
   - Immediately destructure via `LetPattern` (tuple or struct destructuring)
   - Only access fields without escaping the value (SROA will have decomposed these)
     Mark qualifying functions with a metadata flag (e.g., `multivalue_return: true`).

2. **Callee rewrite at WIR build**: When generating a marked function, emit multiple Wasm result types instead of a single struct ref. Replace `struct.new` at return sites with bare stack values.

3. **Call site rewrite at WIR build**: For calls to marked functions, emit `MultiValueLocalBind` instead of `Call` + `StructGet` sequences. The existing `translate_let_pattern()` mechanism can be extended.

4. **Fallback**: If any call site uses the return value as a whole (passes it to another function, stores it, etc.), the function is NOT marked. No cloning — keep it simple.

#### Applicable Syntax Patterns

**Pattern 1: Direct destructuring (primary target)**

```wado
let [lo, hi] = minmax(x, y);
```

Directly maps to `MultiValueLocalBind`. Simplest case.

**Pattern 2: Struct destructuring**

```wado
struct Point { x: i32, y: i32 }
fn origin() -> Point { return Point { x: 0, y: 0 }; }
let { x, y } = origin();
```

Same as tuple but field order follows struct definition order.

**Pattern 3: Field access only (SROA-assisted)**

```wado
let result = minmax(x, y);
use(result.0);
use(result.1);
```

After SROA decomposes `result` into `__sroa_result_0` and `__sroa_result_1`, the pattern becomes equivalent to destructuring. The multi-value analysis can recognize SROA-decomposed call results.

**Pattern 4: Partial use with wildcard**

```wado
let [lo, _] = minmax(x, y);
```

The unused return value is emitted as `drop` in Wasm. Already supported by `MultiValueLocalBind`.

**Not applicable:**

- Mixed call sites (some destructure, some use whole value) — would require function cloning
- CM export boundaries — always need single-value returns per Component Model ABI
- Deeply nested tuples (`[i32, [String, bool]]`) — recursive flattening adds complexity, defer to future work
- Functions with reference-type return fields — `struct.get` on GC refs has different semantics than stack values
- Best limited to small returns (2-4 fields) to avoid Wasm stack depth issues

#### Interaction with Existing Passes

- **SROA**: Decomposes local struct variables. Multi-value return scalarization complements SROA by eliminating the struct at the function boundary. SROA handles the "local" case, multi-value handles the "cross-function" case.
- **Function inlining**: Inlining eliminates the function boundary entirely, making multi-value optimization unnecessary. Multi-value is most valuable for functions too large to inline.
- **Copy propagation**: After multi-value rewrite, trivial copies (`let x = __multivalue_0`) are cleaned up by copy propagation.

### Function Specialization

Create specialized versions of functions for known constant arguments. When constant propagation determines that a function is always called with certain constant arguments, a specialized version can be generated that folds those constants, enabling further optimizations inside the specialized body.

LLVM implements this as `FunctionSpecialization`. It is particularly effective when combined with interprocedural constant propagation.

### Tail Call Optimization via `return_call`

Emit WebAssembly's `return_call` instruction for tail-recursive function calls instead of a regular `call` + `return`. Part of Wasm 3.0 standard.

Detection pattern: `return func(...)` where the call is the direct child of `return`.

Benefits:

- Eliminates stack overflow for deep recursion
- Reduces function call overhead
- Simple implementation — just emit a different Wasm instruction

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

The optimizer could detect these idioms in user code and rewrite them to use single Wasm instructions:

#### Log2 (integer)

```wado
// User writes:
fn ilog2(x: i32) -> i32 {
    let mut n = 0;
    let mut v = x;
    while v > 1 { v = v / 2; n += 1; }
    return n;
}

// Optimizer rewrites to:
fn ilog2(x: i32) -> i32 {
    return 31 - builtin::i32_clz(x);
}
```

#### Power-of-2 Check

```wado
// User writes:
fn is_power_of_2(x: i32) -> bool {
    return x > 0 && (x & (x - 1)) == 0;
}

// With popcnt, this becomes:
fn is_power_of_2(x: i32) -> bool {
    return builtin::i32_popcnt(x) == 1;  // single instruction
}
```

#### Find Lowest Set Bit

```wado
// User writes a loop scanning bits from LSB:
let mut pos = 0;
let mut v = mask;
while v & 1 == 0 { v = v >> 1; pos += 1; }

// Optimizer rewrites to:
let pos = builtin::i32_ctz(mask);  // single instruction
```

#### Bit Count / Hamming Weight

```wado
// User writes:
fn popcount(x: i32) -> i32 {
    let mut count = 0;
    let mut v = x;
    while v != 0 { count += v & 1; v = v >> 1; }
    return count;
}

// Optimizer rewrites to:
fn popcount(x: i32) -> i32 {
    return builtin::i32_popcnt(x);  // single instruction
}
```

#### Integer Width / Bit Length

```wado
// Number of bits needed to represent a value:
fn bit_length(x: i32) -> i32 {
    return 32 - builtin::i32_clz(x);
}
```

These patterns appear frequently in bitmap data structures, hash tables, memory allocators, and compression algorithms. Recognizing them turns O(n) loops into O(1) instructions.

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

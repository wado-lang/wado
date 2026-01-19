# Wado Optimizer

This document describes the optimization features implemented in the Wado compiler and planned optimizations for future development.

## Philosophy: Leverage WebAssembly Native Instructions

**Key Principle:** When WebAssembly provides native instructions for a feature, use them directly rather than implementing complex compiler transformations.

WebAssembly 3.0 (released September 17, 2025) provides a rich set of native instructions that should be preferred over compiler-based transformations:

- **Tail Calls:** Use `return_call`/`return_call_indirect` instead of converting to loops
- **Bulk Operations:** Use `array.copy`, `array.fill`, `memory.copy`, `memory.fill`
- **Bit Operations:** Use `i32.clz`, `i32.ctz`, `i32.popcnt` for efficient bit manipulation
- **Branchless Code:** Use `select` instruction for conditional values without branches
- **GC Operations:** Already using `array.new`, `struct.new`, `array.get`, `array.set`
- **SIMD (future):** Use `v128` vector instructions for parallel operations

This approach:
- Reduces compiler complexity
- Leverages runtime optimizations (JIT compilers optimize Wasm instructions)
- Produces smaller compiled output
- Maintains better portability across Wasm runtimes

See [WebAssembly-Native Instruction Opportunities](#webassembly-native-instruction-opportunities) section below for details.

## Optimization Levels

The Wado compiler supports four optimization levels:

| Level | Flag | Optimizations | Target Use Case |
|-------|------|---------------|-----------------|
| **None** | `-O0` (default) | No optimizations, all features enabled | Debugging |
| **Basic** | `-O1` | DCE + unreachable function removal | Development builds |
| **Full** | `-O2` | Function inlining + DCE + unreachable removal | Server-side production |
| **Size** | `-Os` | Full optimizations + name section stripping | Client-side/frontend |

## Implemented Optimizations

### Dead Code Elimination (DCE)

**Status:** ✅ Implemented (All levels: `-O1`, `-O2`, `-Os`)

**Description:** Removes unreachable functions from the compiled output based on reachability analysis from the entry point (`run()` function).

**Algorithm:**
1. Build call graph from all functions in all modules
2. Perform depth-first search (DFS) reachability analysis from entry point
3. Mark functions as reachable if called transitively from entry point
4. Remove all unreachable functions and their associated string literals

**Implementation:** `wado-compiler/src/optimize.rs`

**Handles:**
- Free functions
- Methods (instance and static)
- Generic methods with monomorphization tracking
- Transitive call chains

**Limitations:**
- Only function-level DCE (no statement/expression-level dead code removal)
- Cannot eliminate dead branches within reachable functions
- Entry point fixed to `run()` function

### Function Inlining

**Status:** ✅ Implemented (`-O2`, `-Os`)

**Description:** Eliminates function call overhead by replacing small pure function calls with their body statements.

**Algorithm:**
1. Identify inline-eligible functions (< 20 statements, pure, non-recursive)
2. Perform inline at call sites with local variable remapping
3. Track string literals from inlined functions
4. Update caller's local count and type table

**Threshold:** Functions with < 20 statements (configurable via `INLINE_THRESHOLD`)

**Implementation:** `wado-compiler/src/optimize.rs:553-978`

**Eligibility Criteria:**
- ✅ Must have a function body
- ✅ NOT from core library (`module_path[0] != "core"`)
- ✅ NO effects (pure functions only)
- ✅ NOT generic functions
- ✅ NOT monomorphized generics
- ✅ NOT recursive
- ✅ NO early returns (return only at end of block)
- ✅ NO reference parameters
- ✅ NO reference return type
- ✅ Statement count < 20

**Limitations:**
- Single-module only (cross-module inlining not supported due to TypeId translation complexity)
- Pure functions only (cannot inline functions with effects)
- No early returns (complex control flow not supported)
- No reference parameters/returns (address-taken locals too complex)
- No generic function inlining (requires complex specialization)
- Core library functions excluded (type dependencies across modules)

### Feature Analysis & Conditional Feature Inclusion

**Status:** ✅ Implemented (All levels)

**Description:** Includes only WASI functions, effects, and builtins that are actually used by reachable code.

**Tracks:**
- **Effect Usage:** Which WASI effects are called (Stdout, Stderr, Environment, MonotonicClock, Exit)
- **WASI Function Usage:** Specific operations on effects (e.g., `Stdout::write_via_stream`)
- **Builtin Usage:** Canonical builtins (stream operations, float-to-string, memory management)
- **Box Primitive Usage:** Which primitive types need box types for references

**Implementation:** `wado-compiler/src/optimize.rs:1168-1460`

**Standard Effects:**
- `Stdout`, `Stderr`, `Environment`, `MonotonicClock` (always available)
- `Exit` (requires explicit usage)

**Canonical Builtins:**
- Stream intrinsics: `stream-new`, `stream-write`, `stream-drop-writable`, `stream-drop-readable`
- Async/task: `task-return`, `waitable-set-new`, `waitable-join`, `waitable-set-wait`, `subtask-drop`
- Memory: `realloc` (always included)
- Float-to-string: `f64_to_buffer`, `f32_to_buffer`

### String Literal DCE

**Status:** ✅ Implemented (All levels)

**Description:** Filters string literals to only include those used by reachable functions.

**Implementation:** Part of DCE analysis, updates `module.string_literals` after reachability analysis

### Recursive Function Detection

**Status:** ✅ Implemented (Used by inlining)

**Description:** Identifies recursive functions using call graph cycle detection to exclude them from inlining.

**Algorithm:**
1. Build call graph (function name → called function names)
2. For each function, check if it can reach itself via call chains
3. Mark as recursive if found in any cycle

**Implementation:** `wado-compiler/src/optimize.rs:342-364`

### Generic Function Handling

**Status:** ✅ Implemented (DCE and feature analysis)

**Description:** Tracks monomorphized generic functions and their base names for correct reachability analysis.

**Implementation:** Generic methods tracked with mangled names; monomorphization metadata preserved

## High-Priority Optimizations (Needed for Sieve Benchmark)

Analysis of `benchmark/sieve.wado` compiled with `-O2` reveals these critical optimization opportunities:

### 1. Strength Reduction ⭐⭐⭐

**Status:** ❌ Not Implemented (Placeholder exists)

**Priority:** HIGH - Critical for sieve benchmark performance

**Description:** Transform expensive loop operations into cheaper equivalent operations.

**Problem Identified in Sieve:**
```wasm
;; Current WAT output (lines 619-620, 631-632):
local.get 8    ;; p
local.get 8    ;; p
i32.mul        ;; p*p (computed TWICE per iteration!)
```

The sieve benchmark computes `p*p` twice per outer loop iteration - once for the loop condition and once to initialize the inner loop variable.

**Proposed Solution:**
Replace multiplication with addition for induction variables:
```wado
// Before (current)
while p * p <= limit {
    let mut multiple = p * p;
    // ...
}

// After (with strength reduction)
let mut p_squared = 4;  // 2*2 initial value
while p_squared <= limit {
    let mut multiple = p_squared;
    // ...
    p_squared += 2*p + 1;  // (p+1)² = p² + 2p + 1
}
```

**Patterns to Detect:**
- `base + counter * step` where counter increments by 1 → replace with accumulator
- `counter * constant` in loops → replace with addition
- `x * x` (squaring) in loops → maintain squared value separately

**Implementation Location:** `wado-compiler/src/optimize.rs:1462-1500` (currently placeholder)

**References:**
- [CSC D70: Compiler Optimization LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf)
- [Cornell CS 6120: Strength Reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/strength-reduction-pass-in-llvm/)

### 2. Loop-Invariant Code Motion (LICM) ⭐⭐⭐

**Status:** ❌ Not Implemented

**Priority:** HIGH - Critical for loop-heavy code

**Description:** Move computations that produce the same value in every iteration to immediately before the loop entry.

**Problem Identified:**
```wado
// Current code has redundant p*p computation
while p * p <= limit {
    if is_prime[p] {
        let mut multiple = p * p;  // p*p computed again!
        // ...
    }
    p += 1;
}
```

**Proposed Solution:**
```wado
// After LICM
let p_squared = p * p;
while p_squared <= limit {
    if is_prime[p] {
        let mut multiple = p_squared;  // Use hoisted value
        // ...
    }
    p += 1;
    p_squared = p * p;  // Update at end of loop
}
```

**Algorithm:**
1. Identify loop boundaries and preheader blocks
2. Find computations that reference only loop-invariant values
3. Check for side effects and dependencies
4. Move eligible computations to preheader block

**Benefits:**
- Eliminates redundant calculations in every iteration
- Works synergistically with strength reduction
- Improves performance for tight loops

**References:**
- [Loop-Invariant Code Motion](https://grokipedia.com/page/Loop-invariant_code_motion)
- [Loop Optimizations Guide](https://johnnysswlab.com/loop-optimizations-interpreting-the-compiler-optimization-report/)

### 3. Bounds Check Elimination ⭐⭐

**Status:** ❌ Not Implemented

**Priority:** HIGH - Array-intensive benchmarks

**Description:** Remove redundant array bounds checks when the compiler can prove indices are within bounds.

**Problem Identified:**
```wado
// Sieve has many array accesses in tight loops
while n <= limit {
    if is_prime[n] {  // bounds check on every iteration
        count += 1;
    }
    n += 1;
}
```

**Proposed Solution:**
Use value range propagation to prove `n` is always within `[0, limit]`:
- Loop condition ensures `n <= limit`
- Array size is `limit + 1`
- Therefore bounds check is redundant

**Algorithm:**
1. Track value ranges for loop variables
2. Compare against array bounds
3. Eliminate checks when range is provably safe

**Benefits:**
- Reduces overhead in array-heavy loops
- Particularly important for safe languages like Wado
- Can provide 10-30% speedup for array-intensive code

**References:**
- [Array Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)
- [Java HotSpot Bounds Check Elimination](https://www.researchgate.net/publication/221302947_Array_bounds_check_elimination_for_the_Java_HotSpot_client_compiler)

## Medium-Priority Optimizations

### 4. Constant Folding ⭐⭐

**Status:** ❌ Not Implemented

**Priority:** MEDIUM - Basic compiler optimization

**Description:** Evaluate constant expressions at compile time rather than runtime.

**Examples:**
```wado
let x = 2 + 3;           // → let x = 5;
let y = 10000000 + 1;    // → let y = 10000001;
let z = true && false;   // → let z = false;
```

**Algorithm:**
1. Identify expressions with all constant operands
2. Evaluate at compile time
3. Replace expression with constant result

**Implementation Notes:**
- Should be applied after each optimization pass
- Works synergistically with constant propagation
- LLVM implicitly folds away constants as instructions are created

**References:**
- [Constant Folding - Wikipedia](https://en.wikipedia.org/wiki/Constant_folding)
- [LLVM Passes Documentation](https://llvm.org/docs/Passes.html)

### 5. Constant Propagation ⭐⭐

**Status:** ❌ Not Implemented

**Priority:** MEDIUM - Enables other optimizations

**Description:** Replace variable uses with their constant values when known.

**Examples:**
```wado
let limit = 10000000;
let size = limit + 1;    // Can propagate limit = 10000000
                         // → let size = 10000001;
```

**Algorithm:**
1. Track assignments of constants to variables
2. For each use of the variable, check if all reaching definitions assign the same constant
3. Replace variable use with constant value

**Variants:**
- **Sparse Conditional Constant Propagation (SCCP):** More powerful variant that handles branches
- **Interprocedural Constant Propagation:** Across function boundaries

**Benefits:**
- Enables constant folding
- Enables dead code elimination (for conditions)
- Reduces runtime variable lookups

**References:**
- [LLVM Constant Propagation](https://releases.llvm.org/2.6/docs/Passes.html)
- [Unlocking Performance with LLVM](https://saliktariq.medium.com/unlocking-performance-potential-exploring-advanced-compiler-optimization-in-llvm-for-c-578b3a3f091a)

### 6. Common Subexpression Elimination (CSE) ⭐⭐

**Status:** ❌ Not Implemented

**Priority:** MEDIUM - Reduces redundant computation

**Description:** Identify identical subexpressions and replace with a single computation.

**Example:**
```wado
let a = x + y * z;
let b = x + y * z;  // Same expression
// After CSE:
let temp = x + y * z;
let a = temp;
let b = temp;
```

**Algorithm:**
1. Build expression tree for each statement
2. Hash expressions to find duplicates
3. Replace duplicates with temporary variable

**Scope:**
- **Local CSE:** Within a basic block
- **Global CSE:** Across basic blocks using dominance analysis

**Benefits:**
- Reduces computation
- Works well with LICM (common expressions in loops)

**References:**
- [CMU Lecture on CSE](https://www.cs.cmu.edu/~janh/courses/411/23/lec/18-peepsub.pdf)
- [Stanford CS143 Optimization](https://web.stanford.edu/class/cs143/lectures/lecture14.pdf)

### 7. Copy Propagation ⭐

**Status:** ❌ Not Implemented

**Priority:** MEDIUM - Reduces indirection

**Description:** Replace uses of a variable that is a copy of another variable with the original.

**Example:**
```wado
let x = a;
let y = x + b;  // x is a copy of a
// After copy propagation:
let x = a;
let y = a + b;  // Use original a
```

**Algorithm:**
1. Identify copy assignments (`x = y`)
2. Track reaching definitions
3. Replace uses of copy with original when safe

**Benefits:**
- Reduces register pressure
- Enables dead store elimination
- Simplifies dataflow

### 8. Peephole Optimization ⭐

**Status:** ❌ Not Implemented

**Priority:** MEDIUM - Catches local inefficiencies

**Description:** Perform pattern matching on small instruction sequences and replace with more efficient equivalents.

**Examples:**
```wado
x = x + 0;      // → (delete)
x = x * 1;      // → (delete)
x = x * 2;      // → x = x << 1;
x = x / 4;      // → x = x >> 2; (for unsigned)
x = not not x;  // → (delete)
```

**Algorithm:**
1. Define pattern rules (pattern → replacement)
2. Scan code with sliding window (3-4 instructions)
3. Match patterns and apply transformations
4. Apply algebraic simplifications

**References:**
- [Peephole Optimization - GeeksforGeeks](https://www.geeksforgeeks.org/compiler-design/peephole-optimization-in-compiler-design/)
- [Peephole Optimization - Wikipedia](https://en.wikipedia.org/wiki/Peephole_optimization)

Note: LLVM implements over 1000 peephole optimizations.

## Lower-Priority Optimizations

### 9. Scalar Replacement of Aggregates (SROA) ⭐

**Status:** ❌ Not Implemented

**Priority:** LOW - Useful for struct-heavy code

**Description:** Break up struct allocations into individual scalar variables when possible.

**Example:**
```wado
struct Point { x: i32, y: i32 }
let p = Point { x: 1, y: 2 };
let sum = p.x + p.y;

// After SROA:
let p_x = 1;
let p_y = 2;
let sum = p_x + p_y;
```

**Benefits:**
- Enables scalar optimizations (CSE, constant propagation)
- Reduces memory allocation
- Better register allocation

**References:**
- [LLVM SROA](https://llvm.org/docs/Passes.html)
- [GCC Scalar Replacement](https://gcc.gnu.org/onlinedocs/gcc/Optimize-Options.html)

### 10. Dead Store Elimination ⭐

**Status:** ❌ Not Implemented

**Priority:** LOW - Cleanup optimization

**Description:** Remove assignments to variables that are never subsequently read.

**Example:**
```wado
let mut x = 1;
x = 2;          // Dead store (overwritten)
x = 3;
return x;
```

**Algorithm:**
1. Perform liveness analysis (which variables are live at each point)
2. Remove stores to variables that are not live after the store

### 11. Tail Call Optimization via `return_call` ⭐⭐

**Status:** ❌ Not Implemented (but straightforward - use Wasm instruction directly)

**Priority:** MEDIUM - Useful for recursive algorithms, leverages native Wasm feature

**Description:** Emit WebAssembly's `return_call` and `return_call_indirect` instructions for tail-recursive function calls instead of converting to loops.

**WebAssembly Support:** Part of Wasm 3.0 standard (September 17, 2025). The `return_call`, `return_call_ref`, and `return_call_indirect` instructions are tail-call variants that first return from the current function before performing the call, with a guarantee that no sequence of nested calls can cause stack overflow.

**Example:**
```wado
fn factorial(n: i32, acc: i32) -> i32 {
    if n == 0 {
        return acc;
    }
    return factorial(n - 1, n * acc);  // Tail call
}
```

Compiles to:
```wat
(func $factorial (param $n i32) (param $acc i32) (result i32)
  (if (i32.eqz (local.get $n))
    (then (return (local.get $acc)))
  )
  (return_call $factorial  ;; Use return_call instead of call + return
    (i32.sub (local.get $n) (i32.const 1))
    (i32.mul (local.get $n) (local.get $acc))
  )
)
```

**Implementation Strategy:**
1. Detect tail-call pattern: `return` statement with function call as direct child
2. Emit `return_call` instruction instead of `call` + `return`
3. Works for both direct calls (`return_call`) and indirect calls (`return_call_indirect`)

**Benefits:**
- Eliminates stack overflow for deep recursion
- Reduces function call overhead
- **Simple implementation** - just emit different Wasm instruction
- Leverages runtime optimizations
- No complex loop transformation needed

**References:**
- [WebAssembly Tail Call Proposal](https://github.com/WebAssembly/tail-call/blob/main/proposals/tail-call/Overview.md)
- [V8 WebAssembly Tail Calls](https://v8.dev/blog/wasm-tail-call)
- [Wasm 3.0 Release](https://webassembly.org/news/2025-09-17-wasm-3.0/)

### 12. Loop Unrolling ⚠️ (Not Recommended for Wasm)

**Status:** ❌ Not Implemented

**Priority:** VERY LOW / NOT RECOMMENDED - Increases code size without clear benefit

**Description:** Replicate loop body multiple times to reduce loop overhead.

**Why Not Recommended for WebAssembly:**
- **Increases instruction count** - More bytes to download, parse, and JIT-compile
- **Wasm runtimes already optimize loops** - V8, SpiderMonkey, and other JIT compilers perform their own loop optimizations
- **Code size matters** - WebAssembly is often used in browsers where download size affects startup time
- **Vectorization should use SIMD** - If parallel operations are needed, use Wasm SIMD (`v128`) instead

**Better Alternative:**
Use `array.fill` for bulk initialization or SIMD instructions for parallel operations:

```wado
// Instead of loop unrolling, use array.fill
let arr: Array<i32> = [];
builtin::array_fill(arr.repr, 0, 0, 100);  // Fill 100 elements with 0

// Or for operations that can be vectorized, use SIMD (future)
// v128.add, v128.mul, etc.
```

**Tradeoffs:**
- ❌ Increases code size significantly
- ❌ May hurt instruction cache performance
- ❌ Benefit highly dependent on target hardware
- ❌ Wasm runtimes already perform loop optimizations
- ❌ Use SIMD or bulk operations instead

### 13. Algebraic Simplification ⭐

**Status:** ❌ Not Implemented

**Priority:** LOW - Often covered by peephole

**Description:** Apply algebraic laws to simplify expressions.

**Examples:**
```wado
x + 0       → x
x * 1       → x
x * 0       → 0
x - x       → 0
x & x       → x
x | x       → x
x ^ x       → 0
```

**Note:** Often implemented as part of peephole optimization.

### 14. Branch Elimination / Dead Branch Removal ⭐

**Status:** ❌ Not Implemented

**Priority:** LOW - Requires constant propagation

**Description:** Remove branches that always take the same path.

**Example:**
```wado
if true {
    println("always");
} else {
    println("never");
}

// After:
println("always");
```

**Requires:** Constant propagation to determine branch conditions

## WebAssembly-Specific Optimizations

These optimizations could be applied at the WAT/Wasm level or delegated to Binaryen's wasm-opt:

### 15. Stack IR Optimizations

**Status:** ❌ Not Implemented

**Description:** Use Binaryen's Stack IR for optimizations tailored to WebAssembly's stack machine.

**Tool:** Binaryen's wasm-opt

**Reference:** [Binaryen Optimizer Cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook)

### 16. Whole-Program Analysis (--gufa)

**Status:** ❌ Not Implemented

**Description:** Infer constant values and exact types in a whole-program manner.

**Benefits:**
- Particularly helpful for WasmGC
- Infers exact types for better optimization

**Tool:** Binaryen's `wasm-opt --gufa`

### 17. Monomorphization

**Status:** Partial (Generic functions are monomorphized, but no optimization)

**Description:** Specialize generic functions for specific type arguments to enable better optimization.

**Current:** Wado generates separate functions for each generic instantiation
**Opportunity:** Optimize each monomorphized variant independently

**Tool:** Binaryen's `wasm-opt --monomorphize`

## WebAssembly-Native Instruction Opportunities

This section details WebAssembly 3.0 instructions that Wado should expose and use directly, rather than implementing complex compiler transformations.

### Already Implemented ✅

**GC Array Operations:**
- `array.new` - Create new array with default value
- `array.new_fixed` - Create array with specific values
- `array.get` - Read array element
- `array.set` - Write array element
- `array.len` - Get array length
- `array.fill` - Fill array slice with value (✅ **exposed as `builtin::array_fill`**)
- `array.copy` - Copy between arrays (✅ **exposed as `builtin::array_copy`**)

**GC Struct Operations:**
- `struct.new` - Create new struct instance
- `struct.get` - Read struct field
- `struct.set` - Write struct field

**Optimization Benefit:** Immutable arrays enable CSE and LICM optimizations - `array.get` on immutable arrays is idempotent and can be deduplicated.

### High Priority - Should Implement 🎯

#### 1. Tail Call Instructions

**Status:** ❌ Not exposed

**Instructions:**
- `return_call` - Direct tail call
- `return_call_indirect` - Indirect tail call
- `return_call_ref` - Reference-based tail call

**Implementation:** Detect `return func(...)` pattern in TIR, emit `return_call` instead of `call` + `return`

**Benefits:**
- Eliminates recursion stack overflow
- Zero-cost recursion
- Simpler than loop transformation

**Reference:** [WebAssembly Tail Call Proposal](https://github.com/WebAssembly/tail-call/blob/main/proposals/tail-call/Overview.md)

#### 2. Branchless Conditional - `select`

**Status:** ❌ Not exposed

**Instruction:** `select` - Choose between two values based on condition without branching

**Example:**
```wado
// Current: generates branch
let max = if a > b { a } else { b };

// With select instruction:
// (select (local.get $a) (local.get $b) (i32.gt_s (local.get $a) (local.get $b)))
```

**Implementation:** Detect ternary-like if expressions, emit `select` for simple value choices

**Benefits:**
- Eliminates branch misprediction penalty
- More compact code
- Better for simple conditionals

**Expose as:** Could be implicit optimization for `if` expressions, or explicit `builtin::select<T>(cond: bool, if_true: T, if_false: T) -> T`

#### 3. Bit Manipulation Instructions

**Status:** ❌ Not exposed

**Instructions:**
- `i32.clz` / `i64.clz` - Count leading zeros
- `i32.ctz` / `i64.ctz` - Count trailing zeros
- `i32.popcnt` / `i64.popcnt` - Count set bits (population count)

**Example Use Cases:**
- `clz` - Find highest set bit, compute log2
- `ctz` - Find lowest set bit, isolate rightmost bit
- `popcnt` - Hamming weight, bit counting algorithms

**Implementation:** Add to `core/builtin.wado`:
```wado
fn i32_clz(x: i32) -> i32;   // Count leading zeros
fn i32_ctz(x: i32) -> i32;   // Count trailing zeros
fn i32_popcnt(x: i32) -> i32;  // Population count
fn i64_clz(x: i64) -> i32;
fn i64_ctz(x: i64) -> i32;
fn i64_popcnt(x: i64) -> i32;
```

**Benefits:**
- Single instruction vs. loop-based implementations
- Essential for bit manipulation algorithms
- Standard in most ISAs (x86 `bsr`/`bsf`/`popcnt`, ARM `clz`)

**Reference:** [WebAssembly 3.0 Instructions](https://webassembly.github.io/spec/core/syntax/instructions.html)

#### 4. Bulk Memory Operations

**Status:** ❌ Not exposed (memory operations)

**Instructions:**
- `memory.fill` - Fill memory region with byte value
- `memory.copy` - Copy memory region (handles overlapping)

**Example:**
```wado
// Zero-initialize buffer
builtin::memory_fill(ptr, 0, size);

// Copy buffer (like memcpy but handles overlap)
builtin::memory_copy(dst, src, size);
```

**Implementation:** Add to `core/builtin.wado`:
```wado
fn memory_fill(dst: i32, value: i32, size: i32);
fn memory_copy(dst: i32, src: i32, size: i32);
```

**Benefits:**
- Optimized by runtime (often maps to platform memcpy/memset)
- Handles overlapping regions correctly
- Much faster than byte-by-byte loops

**Reference:** [WebAssembly Bulk Memory Operations](https://github.com/WebAssembly/bulk-memory-operations/blob/master/proposals/bulk-memory-operations/Overview.md)

### Medium Priority

#### 5. Integer Min/Max Instructions

**Status:** ❌ Not exposed

**Instructions:**
- `i32.min_s` / `i32.min_u` - Signed/unsigned minimum
- `i32.max_s` / `i32.max_u` - Signed/unsigned maximum
- Similar for `i64`

**Note:** Part of extended proposals, check Wasm 3.0 support

#### 6. Sign Extension Instructions

**Status:** Likely already used by codegen

**Instructions:**
- `i32.extend8_s`, `i32.extend16_s` - Sign-extend 8/16 bits to 32
- `i64.extend8_s`, `i64.extend16_s`, `i64.extend32_s` - Sign-extend to 64

### Future: SIMD (v128)

**Status:** ❌ Not planned yet

**Instructions:** `v128` vector operations for parallel integer/float arithmetic

**Use Cases:**
- Array operations (map, filter, reduce)
- Numerical computations
- Image/audio processing

**Benefits:**
- Process 4× i32 or 2× i64 in parallel
- Essential for high-performance numerical code

**Considerations:**
- Adds complexity to type system
- Requires auto-vectorization analysis or explicit SIMD operations
- Best for specific performance-critical code paths

**Reference:** [WebAssembly SIMD](https://github.com/WebAssembly/simd/blob/main/proposals/simd/SIMD.md)

### Summary: Prefer Wasm Instructions Over Compiler Transforms

| Optimization Goal | ❌ Don't Do This | ✅ Do This Instead |
|-------------------|------------------|-------------------|
| Tail recursion | Convert to loop | Emit `return_call` |
| Array initialization | Emit loop | Use `array.fill` |
| Array copy | Emit loop | Use `array.copy` |
| Memory copy | Byte-by-byte loop | Use `memory.copy` |
| Memory zero | Byte-by-byte loop | Use `memory.fill` |
| Conditional value | Branch + PHI | Use `select` |
| Count bits | Loop with shifts | Use `popcnt` |
| Find first bit | Loop with shifts | Use `clz` / `ctz` |
| Min/max | Branch comparison | Use `min` / `max` |
| Loop unrolling | Replicate code 4× | ❌ Don't - increases size |

This approach reduces compiler complexity while producing better code that leverages runtime optimizations.

## Implementation Roadmap

### Phase 0: WebAssembly Native Instructions (Quick Wins)

**Priority:** IMMEDIATE - Simple to implement, immediate benefits

1. **Tail Calls (`return_call`)** - Detect `return func(...)` pattern, emit `return_call`
2. **Bit Manipulation** - Expose `i32.clz`, `i32.ctz`, `i32.popcnt` in `core/builtin.wado`
3. **Bulk Memory** - Expose `memory.fill`, `memory.copy` in `core/builtin.wado`
4. **Select Instruction** - Optimize simple `if` expressions to `select`

**Expected Impact:**
- Enables zero-cost recursion
- Provides essential bit operations
- Faster memory operations
- **Implementation effort: LOW** (mostly exposing existing Wasm instructions)

### Phase 1: Critical Loop Optimizations (High-Priority for Sieve)

1. **Strength Reduction** - Transform expensive loop operations
2. **Loop-Invariant Code Motion** - Hoist invariant computations
3. **Bounds Check Elimination** - Remove redundant array checks

**Expected Impact:** 30-50% speedup on sieve benchmark

### Phase 2: Basic Scalar Optimizations

4. **Constant Folding** - Evaluate constants at compile time
5. **Constant Propagation** - Replace variables with known constants
6. **Copy Propagation** - Reduce variable indirection

**Expected Impact:** Enables other optimizations, 5-15% overall improvement

### Phase 3: Advanced Analysis

7. **Common Subexpression Elimination** - Eliminate duplicate computations
8. **Peephole Optimization** - Local pattern-based improvements
9. **Dead Store Elimination** - Remove unused assignments

**Expected Impact:** 5-10% improvement, smaller code size

### Phase 4: Specialized Optimizations

10. **Scalar Replacement of Aggregates** - Break up structs
11. **Dead Store Elimination** - Remove unused assignments
12. **Algebraic Simplification** - Apply algebraic laws

**Expected Impact:** Workload-dependent, 5-15% improvement on struct-heavy code

## Testing Strategy

1. **Golden Fixtures:** Extend `tests/fixtures.golden/*.lowered.wado` to capture optimized TIR
2. **Benchmark Suite:** Expand benchmarks beyond sieve (mandelbrot, count-prime)
3. **Correctness Tests:** Ensure optimizations preserve semantics
4. **Performance Regression:** Track benchmark performance over time

## References

### Loop Optimizations
- [CSC D70: Compiler Optimization LICM](http://www.cs.toronto.edu/~pekhimenko/courses/cscd70-w18/docs/Lecture%205%20%5BLICM%20and%20Strength%20Reduction%5D%2002.08.2018.pdf)
- [Loop-Invariant Code Motion](https://grokipedia.com/page/Loop-invariant_code_motion)
- [Loop Optimizations Guide](https://johnnysswlab.com/loop-optimizations-interpreting-the-compiler-optimization-report/)
- [Cornell CS 6120: Loop Reduction](https://www.cs.cornell.edu/courses/cs6120/2019fa/blog/loop-reduction/)

### LLVM Optimizations
- [LLVM's Analysis and Transform Passes](https://llvm.org/docs/Passes.html)
- [LLVM Constant Propagation](https://releases.llvm.org/2.6/docs/Passes.html)
- [Unlocking Performance with LLVM](https://saliktariq.medium.com/unlocking-performance-potential-exploring-advanced-compiler-optimization-in-llvm-for-c-578b3a3f091a)
- [How LLVM Optimizes](https://blog.regehr.org/archives/1603)

### Bounds Check Elimination
- [Array Bounds Check Elimination in CLR](https://learn.microsoft.com/en-us/archive/blogs/clrcodegeneration/array-bounds-check-elimination-in-the-clr)
- [Java HotSpot Bounds Check Elimination](https://www.researchgate.net/publication/221302947_Array_bounds_check_elimination_for_the_Java_HotSpot_client_compiler)
- [Scalar Replacement of Aggregates](https://www.researchgate.net/publication/261615418_Inter-iteration_Scalar_Replacement_Using_Array_SSA_Form)

### Peephole and Local Optimizations
- [CMU Lecture on Peephole and CSE](https://www.cs.cmu.edu/~janh/courses/411/23/lec/18-peepsub.pdf)
- [Peephole Optimization - GeeksforGeeks](https://www.geeksforgeeks.org/compiler-design/peephole-optimization-in-compiler-design/)
- [Peephole Optimization - Wikipedia](https://en.wikipedia.org/wiki/Peephole_optimization)

### WebAssembly Instructions and Features
- [Wasm 3.0 Release (September 17, 2025)](https://webassembly.org/news/2025-09-17-wasm-3.0/)
- [WebAssembly 3.0 Instructions Specification](https://webassembly.github.io/spec/core/syntax/instructions.html)
- [WebAssembly Tail Call Proposal](https://github.com/WebAssembly/tail-call/blob/main/proposals/tail-call/Overview.md)
- [V8 WebAssembly Tail Calls](https://v8.dev/blog/wasm-tail-call)
- [WebAssembly Bulk Memory Operations](https://github.com/WebAssembly/bulk-memory-operations/blob/master/proposals/bulk-memory-operations/Overview.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc/blob/main/proposals/gc/Overview.md)
- [A new way to bring GC languages to WebAssembly - V8](https://v8.dev/blog/wasm-gc-porting)
- [WebAssembly SIMD](https://github.com/WebAssembly/simd/blob/main/proposals/simd/SIMD.md)
- [V8 WebAssembly SIMD](https://v8.dev/features/simd)

### WebAssembly Optimizations and Tools
- [Binaryen Optimizer Cookbook](https://github.com/WebAssembly/binaryen/wiki/Optimizer-Cookbook)
- [Mastering WebAssembly Optimization](https://compile7.org/decompile/webassembly-optimization-strategies)
- [V8 Speculative WebAssembly Optimizations](https://v8.dev/blog/wasm-speculative-optimizations)
- [Compiling to Wasm with Binaryen](https://web.dev/articles/binaryen)

### General Compiler Optimization
- [Optimizing Compiler - Wikipedia](https://en.wikipedia.org/wiki/Optimizing_compiler)
- [GCC Optimization Options](https://gcc.gnu.org/onlinedocs/gcc/Optimize-Options.html)
- [Can You Trust a Compiler to Optimize?](https://matklad.github.io/2023/04/09/can-you-trust-a-compiler-to-optimize-your-code.html)

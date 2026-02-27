# WEP: Optimize Float-to-String Performance

## Context

The FTS (float-to-string) benchmark converts 500,000 `f64` values to fixed-precision decimal strings (`{x:0.6f}`). Current results:

| Runtime             | Time (ms) | Relative  |
| ------------------- | --------- | --------- |
| Zig (-OReleaseFast) | 27        | 1.00x     |
| Rust (rustc -O)     | 38        | 1.41x     |
| C (gcc -O3)         | 61        | 2.26x     |
| **Wado**            | 72,999    | 2,703.67x |

The target is ~100x (≈2,700 ms). The fpfmt algorithm itself is efficient (pure integer arithmetic, 128-bit multiply via `i64.mul_wide_u`). The bottleneck is in how the result string is constructed and how the formatting infrastructure is used.

## Analysis of the Hot Loop

The benchmark hot loop (500K iterations) does:

```wado
let s = `{x:0.6f}`;
```

which desugars to:

```
let mut __r = String { repr: array_new<u8>(16), used: 0 };     // (1) GC alloc
let mut __f = Formatter { fill: ' ', align: Right, ..., buf: &mut __r }; // (2) GC alloc
fmt_f64_fixed(x, 6, &mut __f);                                  // (3) formatting
break __tmpl: __r;                                               // (4) result
```

### Problem 1: Per-Iteration GC Allocations (Critical)

Every iteration allocates:

- A `String` struct (2 fields) + its backing `array<u8>(16)` — **2 GC objects**
- A `Formatter` struct (8 fields) — **1 GC object**

That's **1.5 million GC allocations** for 500K iterations. Each `struct.new` is a GC heap allocation in WasmGC.

**Fix: Template String Buffer Reuse**

When a template string expression is inside a loop body and the result is consumed before the next iteration (not captured by reference or closure), the compiler can **reuse the String buffer** across iterations by resetting `used = 0` instead of allocating a new String. Similarly, the Formatter can be reset rather than reallocated.

This is a TIR-level optimization: detect `__tmpl` blocks inside loops, and if the resulting String does not escape the loop body, hoist the allocation before the loop and reset inside.

Before (current):

```
loop {
    let __r = String { repr: array_new<u8>(16), used: 0 };
    let __f = Formatter { ..., buf: &mut __r };
    fmt_f64_fixed(x, 6, &mut __f);
    // use __r ...
}
```

After (optimized):

```
let __r = String { repr: array_new<u8>(16), used: 0 };
let __f = Formatter { ..., buf: &mut __r };
loop {
    __r.used = 0;  // reset string, reuse backing array
    __f.precision = 6; __f.width = 0; // reset formatter fields
    fmt_f64_fixed(x, 6, &mut __f);
    // use __r ...
}
```

**Expected impact: ~3x speedup** (eliminates 1.5M GC allocations → 2 total).

### Problem 2: `String::append` Uses Byte-by-Byte Copy (Critical)

`String::append` copies bytes in a loop:

```wado
for let mut i = 0; i < other_len; i += 1 {
    let byte = builtin::array_get_u8(other_repr, i);
    builtin::array_set_u8(self.repr, self.used + i, byte);
}
```

The same pattern appears in `String::grow` — it copies the old backing array to the new one byte-by-byte.

`builtin::array_copy` exists and is already used in `Array<T>` methods, but `String` does not use it.

**Fix: Use `array_copy` in String operations**

```wado
// String::append
builtin::array_copy::<u8>(self.repr, self.used, other_repr, 0, other_len);

// String::grow
builtin::array_copy::<u8>(new_repr, 0, self.repr, 0, self.used);
```

This applies to all String methods that do byte-by-byte copy: `append`, `grow`, `concat`, `slice`, `internal_append_from_memory`.

**Expected impact: ~2-5x speedup** for string operations. `array.copy` is a single Wasm instruction that runtimes (V8, wasmtime) optimize as a `memcpy`.

### Problem 3: `String::append` with Short Constants Allocates GC Strings (High)

In `fmt_f64_fixed`, appending "-" or "+" creates a temporary String:

```
String::append(f.buf, String { repr: array.new_data<u8>[21](0, 1), used: 1 });
```

Every 1-byte append allocates a `String` struct and a `array<u8>` from a data segment. For the FTS benchmark, this happens per iteration for the sign check.

**Fix: Library Call Optimization — `append(short_const)` → `append_char` sequence**

This is already described in `docs/optimizer.md` as a planned optimization. When `String::append` is called with a constant string of 4 bytes or fewer, rewrite it to a sequence of `append_char` calls (or `append_byte` for ASCII). No GC allocation needed.

```
// Before
String::append(buf, String { repr: array.new_data<u8>(..., 1), used: 1 });
// After
String::append_char(buf, '.');  // or a new append_byte(u8) for even less overhead
```

Even better: add a `String::append_byte(u8)` method that avoids the multi-branch UTF-8 encoding in `append_char`:

```wado
pub fn append_byte(&mut self, byte: u8) {
    if builtin::unlikely(self.used >= builtin::array_len(self.repr)) {
        self.grow(self.used + 1);
    }
    builtin::array_set_u8(self.repr, self.used, byte);
    self.used += 1;
}
```

**Expected impact: ~1.5x speedup** (eliminates hundreds of thousands of temporary GC String allocations).

### Problem 4: Redundant `short()` Call in `fmt_f64_fixed` (Medium)

`fmt_f64_fixed` calls `short(abs_f)` first (to determine the decimal position), then calls `fixed_width(abs_f, clamped)` (which internally calls `unpack64` and `mul_pow10` again). Both call chains perform:

- `unpack64(f)` — IEEE 754 bit extraction
- `mul_pow10(p)` — 128-bit multiply with power-of-10 decomposition (coarse×fine)
- `uscale(m, pm_hi, pm_lo, s)` — 128-bit multiply

This means the 128-bit multiply chain is executed **twice per float**. The second call (`fixed_width`) is necessary because it computes a different number of digits, but the `unpack64` and `mul_pow10` computations are shared.

**Fix: Fused `fixed_width_from_short`**

Create a specialized function that takes the already-computed `short` result and derives the `fixed_width` result without re-unpacking or re-computing the power-of-10 multiplication:

```wado
fn fmt_f64_fixed_fast(value: f64, precision: i32, f: &mut Formatter) {
    let [is_special, is_neg, kind] = check_special(value);
    // ... special handling ...
    let abs_f = if is_neg { -value } else { value };
    let unpack = unpack64(abs_f);
    let m = unpack.mantissa;
    let e = unpack.exponent;

    // Compute short representation
    let short_result = short_from_unpack(m, e);
    let short_nd = digits(short_result.digits);
    let decimal_pos = short_nd + short_result.exponent;
    let int_digits = if decimal_pos > 0 { decimal_pos } else { 1 };
    let total_digits = int_digits + precision;
    let clamped = /* ... */;

    // Compute fixed_width reusing the same mantissa/exponent
    let result = fixed_width_from_unpack(m, e, clamped);
    // ... write digits ...
}
```

Alternatively, since `short` and `fixed_width` both start with `unpack64` which is fast, the more impactful refactor is to inline `unpack64` (which the optimizer should already be doing at O2 since it's small). Check whether it actually gets inlined.

**Expected impact: ~1.3x speedup** (eliminates redundant `unpack64` + `mul_pow10` per float).

### Problem 5: `Formatter::apply_padding` Overhead (Medium)

Every `fmt_f64_fixed` call ends with `Formatter::apply_padding(f, mark)`. In the FTS benchmark, `width=0`, so `apply_padding` immediately returns after checking `self.width <= 0`. But this is still a function call with struct field access.

Looking at the WIR, `apply_padding` is not inlined (it has loops inside for the padding logic). The early-exit check should be inlined or the call should be eliminated when the Formatter's `width` is known to be ≤ 0.

**Fix: Constant propagation through Formatter fields**

When the Formatter is constructed with `width: 0` (or `width: -1`) as a literal, the optimizer should recognize that `apply_padding` will always early-return and eliminate the call. This requires IPSCCP or function specialization.

A simpler alternative: make `apply_padding` `#[inline]` or split it into a fast-path check + slow-path call:

```wado
#[inline(always)]
pub fn apply_padding(&mut self, start_pos: i32) {
    if self.width <= 0 { return; }
    self.apply_padding_slow(start_pos);
}
```

**Expected impact: ~1.1x speedup** (eliminates 500K function calls to `apply_padding`).

### Problem 6: `Formatter` Struct Not SROA'd (Medium)

The `Formatter` struct has 8 fields and is passed by `ref` to formatting functions. SROA cannot decompose it because it escapes to function calls (hard escape). This means every field access goes through a GC struct indirection.

For the FTS benchmark, most Formatter fields are constants (`fill=' '`, `align=Right`, `sign_plus=false`, `alternate=false`, `zero_pad=false`, `width=0`). Only `precision=6` and `buf=&mut __r` vary (and even those are constant within the loop).

**Fix: Argument Promotion for Formatter**

The `fmt_f64_fixed` function only accesses these Formatter fields:

- `f.buf` (to get the String buffer)
- `f.sign_plus` (to check sign)
- `f.mark()` → `f.buf.used` (to get current position)
- `f.apply_padding(mark)` (width/alignment)

Instead of passing the entire Formatter struct, pass the buffer directly. For the template string case with no width/alignment specifiers, the compiler could generate a specialized call path:

```
// Instead of:
fmt_f64_fixed(x, 6, &mut __f);
// Generate:
fmt_f64_fixed_simple(x, 6, &mut __r);  // no padding, no sign, just digits
```

This is a form of function specialization based on constant Formatter flags.

**Expected impact: ~1.2x speedup** (eliminates Formatter allocation and indirection).

### Problem 7: `write_decimal_prec` Byte Shifting Loop (Low-Medium)

`write_decimal_prec` inserts a decimal point by first writing all digits, then shifting bytes rightward to make room for the '.':

```
// Write digits
write_digits_at(buf, start, d, nd);
// Shift bytes right to insert '.'
for i = start + nd; i > start + decimal_pos; i -= 1 {
    buf.set_byte(i, buf.get_byte(i - 1));
}
buf.set_byte(start + decimal_pos, '.');
```

This is O(nd) byte shifting per format. For 8-character output ("0.123456"), this shifts ~6 bytes.

**Fix: Use `array.copy` for byte shifting**

```wado
// Instead of byte-by-byte shift loop:
let shift_start = start + decimal_pos;
let shift_len = nd - decimal_pos;  // typically ~6 for 0.6f
builtin::array_copy::<u8>(buf.repr, shift_start + 1, buf.repr, shift_start, shift_len);
buf.set_byte(shift_start, '.');
```

`array.copy` handles overlapping regions correctly (like `memmove`).

**Expected impact: ~1.1x speedup** (small per-call but 500K iterations).

### Problem 8: `POW10` Array Access in `fixed_width` (Low)

`fixed_width` does `Array<u64>^IndexValue::index_value(POW10, n)` which includes bounds checking. Since `n` is clamped to 1..18 and POW10 has 20 elements, the bounds check is always unnecessary.

**Fix: Use unchecked array access or constant table**

Since POW10 values are compile-time constants (10^0 through 10^19), the optimizer could recognize `POW10[n]` where `n` is bounded and replace with a `switch` on `n` or direct computation.

**Expected impact: negligible** (bounds check is a single comparison, branch predictor handles it).

## Summary and Priority

| # | Optimization                               | Category               | Expected Impact | Difficulty  |
| - | ------------------------------------------ | ---------------------- | --------------- | ----------- |
| 1 | Template string buffer reuse in loops      | TIR optimizer          | ~3x             | Medium      |
| 2 | `String::append`/`grow` → `array.copy`     | Stdlib fix             | ~2-5x           | Easy        |
| 3 | Short-constant `append` → `append_byte`    | TIR optimizer / Stdlib | ~1.5x           | Easy-Medium |
| 4 | Eliminate redundant `unpack64`/`mul_pow10` | Stdlib refactor        | ~1.3x           | Medium      |
| 5 | Inline `apply_padding` fast path           | Stdlib + optimizer     | ~1.1x           | Easy        |
| 6 | Specialize Formatter for simple cases      | TIR optimizer          | ~1.2x           | Hard        |
| 7 | `array.copy` for decimal point insertion   | Stdlib fix             | ~1.1x           | Easy        |
| 8 | Unchecked POW10 access                     | Optimizer              | negligible      | Easy        |

**Combined expected impact**: ~10-20x speedup, bringing FTS from ~2700x to ~135-270x.

## Recommended Implementation Order

### Phase 1: Quick Wins (target: ~5-10x improvement)

1. **`String::append` → `array.copy`** (#2) — one-line change in `string.wado`, immediate global benefit
2. **`String::grow` → `array.copy`** (#2) — same file, same pattern
3. **`String::append_byte` method** (#3) — add new method, use for single-byte appends
4. **Byte-shift → `array.copy`** (#7) — in `fpfmt.wado`, `write_decimal` and `write_decimal_prec`
5. **`apply_padding` fast path inline** (#5) — split into inline check + slow path

### Phase 2: Optimizer Improvements (target: additional ~3-5x)

6. **Template string buffer reuse** (#1) — new optimizer pass
7. **Library call optimization for short constants** (#3) — optimizer rewrites `append(1-byte)` → `append_byte`

### Phase 3: Algorithmic (target: additional ~1.3x)

8. **Fused short+fixed_width** (#4) — refactor fpfmt.wado to share unpack/mul_pow10 results

## Non-Goals

- Changing the fpfmt algorithm — it is already optimal (Russ Cox's unrounded scaling)
- SIMD — not applicable to this workload (sequential float formatting)
- Wasm-opt / Binaryen — external tool dependency; prefer compiler-native optimizations
- Multi-threading — the benchmark is inherently sequential

## Consequences

- Phase 1 changes are pure stdlib improvements with no optimizer changes needed
- `String::append` using `array.copy` benefits ALL string operations, not just FTS
- Template buffer reuse requires escape analysis on `__tmpl` blocks, which is a new optimizer concept
- The `Formatter` specialization (#6) is architecturally complex and may not be worth the effort given the other improvements

# WEP: Range Object

## Context

Wado needs a range object to support common patterns like counted iteration, membership testing, and array slicing. Currently, counted loops require C-style `for` syntax:

```wado
for let mut i = 0; i < 10; i += 1 {
    println(`{i}`);
}
```

A range object would enable the more idiomatic:

```wado
for let i of 0..<10 {
    println(`{i}`);
}
```

The operator precedence WEP reserves `..` and `..=` at level 14; this WEP revises the syntax to `..<` (half-open) and `..=` (inclusive) for clarity. The iterator traits WEP sketches a non-generic `Range` struct for `i32` only. This WEP defines the full design.

### Language Survey

#### Rust

Rust has the most comprehensive range system with six distinct types:

| Type | Syntax | Interval | Iterable |
|------|--------|----------|----------|
| `Range<Idx>` | `a..b` | [a, b) | Yes (when `Idx: Step`) |
| `RangeInclusive<Idx>` | `a..=b` | [a, b] | Yes (when `Idx: Step`) |
| `RangeFrom<Idx>` | `a..` | [a, +inf) | Yes (infinite) |
| `RangeTo<Idx>` | `..b` | (-inf, b) | No |
| `RangeToInclusive<Idx>` | `..=b` | (-inf, b] | No |
| `RangeFull` | `..` | (-inf, +inf) | No |

Key design elements:
- All types are generic structs in `std::ops`
- The `Step` trait (unstable) enables integer/char iteration
- `RangeBounds` trait unifies all range types for generic code accepting any range
- `RangeInclusive` has an internal `exhausted` flag to handle `T::MAX` correctly
- Ranges are used for iteration, slicing, membership testing (`contains`), and pattern matching
- `Range` is not `Copy` because it implements `Iterator` (mutable state)
- No negative step — use `.rev()` for reverse iteration
- Zero-cost: compiles to the same code as a C-style loop

**Trade-offs**: Six types add complexity. The `Step` trait is unstable after years. `RangeInclusive`'s `exhausted` flag adds overhead. Not having negative step requires `.rev()`.

#### Swift

Swift has five range types with different operators:

| Type | Syntax | Interval |
|------|--------|----------|
| `Range<Bound>` | `a..<b` | [a, b) |
| `ClosedRange<Bound>` | `a...b` | [a, b] |
| `PartialRangeFrom<Bound>` | `a...` | [a, +inf) |
| `PartialRangeThrough<Bound>` | `...b` | (-inf, b] |
| `PartialRangeUpTo<Bound>` | `..<b` | (-inf, b) |

Key design elements:
- All require `Bound: Comparable` — any comparable type can form a range
- Only iterable when `Bound: Strideable & Comparable` (integers, not floats by default)
- `stride(from:to:by:)` and `stride(from:through:by:)` provide stepping
- Used for for-in loops, subscript slicing, switch/case, and `contains`
- Separate `RangeExpression` protocol unifies all range types

**Trade-offs**: Two operators (`..<` and `...`) are visually similar — easy to confuse. Five types, like Rust, add complexity. `stride` is a separate concept from ranges.

#### Go

Go takes a fundamentally different approach — `range` is a keyword, not a type:

- `for i := range 10` (Go 1.22+) iterates 0..9
- `for i, v := range slice` iterates over collections
- `for k, v := range funcIterator` (Go 1.23+) supports user-defined iterators via push-style callbacks
- No `Range` type — ranges are purely syntactic
- No lazy ranges or combinators in the standard library

**Trade-offs**: Maximum simplicity. No range objects to store, pass, or compose. But no way to express ranges as values, no slicing with ranges, no generic range abstraction.

#### Zig

Zig treats ranges as syntax, not types:

- `for (0..n)` iterates in `for` loops (exclusive end, `usize` only)
- `arr[a..b]` for slicing (produces fat-pointer slices)
- `'a'...'z'` in `switch` for inclusive range matching (three dots)
- Multi-sequence `for` with strict same-length requirement
- No first-class `Range` type — community workarounds use custom structs

**Trade-offs**: Simple and zero-cost. But `usize`-only for loop counters, no generic ranges, and ranges cannot be passed as values.

### Summary

| | Rust | Swift | Go | Zig |
|---|---|---|---|---|
| Range as type | Yes (6 types) | Yes (5 types) | No (keyword) | No (syntax) |
| Half-open syntax | `a..b` | `a..<b` | N/A | `a..b` |
| Inclusive syntax | `a..=b` | `a...b` | N/A | `a...b` (switch only) |
| Generic | Yes | Yes | N/A | No (usize only) |
| Iterable | Via Step trait | Via Strideable | Built-in | Built-in |
| Slicing | Yes | Yes | No | Yes |
| Contains | Yes | Yes | No | No |
| Pattern matching | Yes (inclusive only) | Yes (switch/case) | No | Yes (switch) |
| Custom step | `.step_by(n)` | `stride(from:to:by:)` | N/A | No |
| Reverse iteration | `.rev()` | `.reversed()` | N/A | No |

## Decision

### Range Types

Wado defines two range types as generic structs in `core:prelude`:

```wado
/// Half-open range [start, end)
pub struct Range<T> {
    pub start: T,
    pub end: T,
}

/// Inclusive range [start, end]
pub struct RangeInclusive<T> {
    pub start: T,
    pub end: T,
}
```

**Why two types, not six**: Wado targets pragmatic simplicity. The primary use cases (iteration and slicing) are served by `Range` and `RangeInclusive`. Partial ranges (`a..`, `..b`, `..`) add complexity for marginal benefit — Wado already has `arr.slice(start, end)` and `arr.len()` for these cases. If partial ranges prove necessary, they can be added later without breaking changes.

**Why generic**: Unlike Zig's `usize`-only limitation, generic ranges allow natural expressions like `for let c of 'a'..='z'` and `if (0.0..<1.0).contains(x)`. The type parameter is inferred from the operands.

### Syntax

```wado
// Half-open range [start, end)
0..<10             // Range<i32>
0 as i64..<100     // Range<i64>

// Inclusive range [start, end]
0..=10            // RangeInclusive<i32>
'a'..='z'         // RangeInclusive<char>

// With expressions
0..<arr.len()      // Range<i32>
(x + 1)..<(y - 1) // Range<i32>
```

Range operators `..<` and `..=` sit at precedence level 14 (between logical OR and assignment). They are non-associative — `a..<b..<c` is a compile error.

**Why `..<` and `..=`**:
- **Both operators are explicit about the bound**: `..<` clearly reads as "less than" (exclusive end), `..=` clearly reads as "equals" (inclusive end). There is no ambiguous "bare" `..` operator — both sides state the boundary rule.
- **Reduces off-by-one bugs**: With `..` and `..=`, forgetting `=` silently gives a different range. With `..<` and `..=`, there is no "default" form to accidentally misuse.
- **Frees `..` for other syntax**: Wado already uses `..` for struct rest patterns (`let { name, .. } = p`). Keeping `..` out of range syntax avoids ambiguity.
- **Familiar components**: `..<` is from Swift; `..=` is from Rust. The combination takes the clearest element from each.
- `...` (Swift/Zig inclusive) is avoided because it conflicts with potential variadic syntax and is visually similar to `..`.

### Iteration

Ranges implement `Iterator` and `IntoIterator` for integer and char types via bounded impl blocks.

#### Step Trait

A new `Step` trait defines how to advance a value by one:

```wado
/// Types that can be incremented by one step (for range iteration).
pub trait Step {
    /// Advance the value by one step. Returns null if overflow would occur.
    fn next_step(&self) -> Option<Self>;
}
```

Built-in implementations for all integer types and `char`:

```wado
impl Step for i32 {
    fn next_step(&self) -> Option<i32> {
        if *self == i32::MAX { return null; }
        return Option::Some(*self + 1);
    }
}

// Similarly for i8, i16, i64, u8, u16, u32, u64, i128, u128

impl Step for char {
    fn next_step(&self) -> Option<char> {
        let code = *self as u32 + 1;
        return char::from_u32(code);
    }
}
```

#### Iterator Implementations

```wado
impl Iterator for Range<T: Step + Ord> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.start >= self.end {
            return null;
        }
        let current = self.start;
        if let Some(next) = current.next_step() {
            self.start = next;
        } else {
            // Overflow — make the range empty so iteration stops
            self.start = self.end;
        }
        return Option::<T>::Some(current);
    }
}

impl Iterator for RangeInclusive<T: Step + Ord> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.start > self.end {
            return null;
        }
        let current = self.start;
        if self.start == self.end {
            // Last element — advance start past end to mark as exhausted
            if let Some(next) = current.next_step() {
                self.start = next;
            } else {
                // At T::MAX — use a sentinel state
                // We set start > end by retreating end if possible,
                // or by relying on the start > end check above next time
                // Implementation detail: compiler can use an exhausted flag internally
                self.start = self.end;
                // Mark that we already yielded this element
                // (handled via an internal exhausted flag in the compiler)
            }
            return Option::<T>::Some(current);
        }
        if let Some(next) = current.next_step() {
            self.start = next;
        }
        return Option::<T>::Some(current);
    }
}
```

**`RangeInclusive` and T::MAX**: Following Rust's lead, `RangeInclusive` must correctly handle the case where `end == T::MAX` (e.g., `0u8..=255`). The compiler adds an internal `exhausted: bool` flag to `RangeInclusive` that is not visible in the struct definition but tracked in codegen. This is a pragmatic compromise — exposing the flag would leak implementation details; hiding it keeps the API clean.

#### IntoIterator

Both range types are their own iterators (Range itself implements Iterator), so IntoIterator returns self:

```wado
impl IntoIterator for Range<T: Step + Ord> {
    type Item = T;
    type Iter = Range<T>;

    fn into_iter(&self) -> Range<T> {
        return *self;  // Copy (value semantics)
    }
}

impl IntoIterator for RangeInclusive<T: Step + Ord> {
    type Item = T;
    type Iter = RangeInclusive<T>;

    fn into_iter(&self) -> RangeInclusive<T> {
        return *self;
    }
}
```

Since ranges have value semantics, copying a range into the iterator is safe and cheap.

### Usage: For-Of Loops

```wado
// Count from 0 to 9
for let i of 0..<10 {
    println(`{i}`);
}

// Count from 1 to 10 (inclusive)
for let i of 1..=10 {
    println(`{i}`);
}

// Character range
for let c of 'a'..='z' {
    print(`{c}`);
}
// Output: abcdefghijklmnopqrstuvwxyz

// With iterator combinators
let sum = (1..=100).fold(0, |acc: i32, x: i32| acc + x);  // 5050

let evens = (0..<20).filter(|x: i32| x % 2 == 0).collect();
// [0, 2, 4, 6, 8, 10, 12, 14, 16, 18]
```

### Membership Testing

All ranges support `contains` for any `Ord` type, including non-iterable types like floats:

```wado
impl Range<T: Ord> {
    /// Returns true if the value is within [start, end).
    pub fn contains(&self, value: &T) -> bool {
        return *value >= self.start && *value < self.end;
    }

    /// Returns true if the range contains no elements.
    pub fn is_empty(&self) -> bool {
        return self.start >= self.end;
    }
}

impl RangeInclusive<T: Ord> {
    /// Returns true if the value is within [start, end].
    pub fn contains(&self, value: &T) -> bool {
        return *value >= self.start && *value <= self.end;
    }

    /// Returns true if the range contains no elements.
    pub fn is_empty(&self) -> bool {
        return self.start > self.end;
    }
}
```

```wado
// Float membership testing (not iterable, but contains works)
let unit_range = 0.0..<1.0;
if unit_range.contains(&x) {
    println("x is in [0, 1)");
}

// Integer membership testing
if (0..<256).contains(&code) {
    println("valid byte");
}

// Comparison chaining is often clearer for simple cases:
if 0 <= x && x < 256 {
    // equivalent to (0..<256).contains(&x)
}
// Or with Wado's comparison chaining:
if 0 <= x < 256 {
    // same
}
```

### Array Slicing

Ranges can be used as array index types to produce slices:

```wado
let arr: Array<i32> = [10, 20, 30, 40, 50];

// Range slicing (produces ArraySlice<T>)
let slice = arr[1..<4];    // ArraySlice<i32> containing [20, 30, 40]
let slice = arr[2..=4];   // ArraySlice<i32> containing [30, 40, 50]
```

This is implemented via `IndexValue` trait:

```wado
impl IndexValue<Range<i32>> for Array<T> {
    type Output = ArraySlice<T>;

    fn index_value(&self, range: Range<i32>) -> ArraySlice<T> {
        return self.slice(range.start, range.end);
    }
}

impl IndexValue<RangeInclusive<i32>> for Array<T> {
    type Output = ArraySlice<T>;

    fn index_value(&self, range: RangeInclusive<i32>) -> ArraySlice<T> {
        return self.slice(range.start, range.end + 1);
    }
}
```

### Display and Inspect

```wado
impl Display for Range<T: Display> {
    fn fmt(&self, f: &mut Formatter) {
        f.write(`{self.start}..<{self.end}`);
    }
}

impl Display for RangeInclusive<T: Display> {
    fn fmt(&self, f: &mut Formatter) {
        f.write(`{self.start}..={self.end}`);
    }
}
```

```wado
let r = 0..<10;
println(`{r}`);   // "0..<10"

let r = 1..=5;
println(`{r}`);   // "1..=5"
```

### Eq for Ranges

```wado
impl Eq for Range<T: Eq> {
    fn eq(&self, other: &Self) -> bool {
        return self.start == other.start && self.end == other.end;
    }
}

impl Eq for RangeInclusive<T: Eq> {
    fn eq(&self, other: &Self) -> bool {
        return self.start == other.start && self.end == other.end;
    }
}
```

### What Is NOT Included (and Why)

#### No partial ranges (`a..<`, `..<b`, `..`)

Partial ranges add three more types for limited benefit. The primary use case is slicing sugar:

```wado
// These are NOT supported:
arr[2..<]     // use arr.slice(2, arr.len()) instead
arr[..<3]     // use arr.slice(0, 3) instead
arr[..]       // use arr directly (already value semantics)
```

If demand arises, partial ranges can be added as separate types without breaking existing code.

#### No `step_by` method

Custom step sizes add complexity. Use C-style `for` for non-unit steps:

```wado
// Instead of (0..<100).step_by(2)
for let mut i = 0; i < 100; i += 2 {
    // every other number
}
```

A `step_by` combinator can be added later as a method on iterators (not range-specific).

#### No reverse iteration method

Reverse ranges can use C-style `for`:

```wado
// Instead of (0..<10).rev()
for let mut i = 9; i >= 0; i -= 1 {
    println(`{i}`);
}
```

A general `.rev()` iterator combinator can be added later to the `Iterator` trait.

#### No pattern matching with ranges

Range patterns in `match` arms (e.g., `0..=9 => "digit"`) require parser and pattern matching infrastructure changes that are orthogonal to the range object design. This can be addressed in a separate WEP.

### Wasm GC Representation

`Range<T>` and `RangeInclusive<T>` are struct types. For primitive `T`, the compiler monomorphizes them to simple Wasm GC structs:

```wat
;; Range<i32>
(type $Range_i32 (struct (field $start i32) (field $end i32)))

;; RangeInclusive<i32> (with exhausted flag for Iterator)
(type $RangeInclusive_i32 (struct (field $start i32) (field $end i32) (field $exhausted i32)))
```

For iteration, the `exhausted` flag is only present in `RangeInclusive` and only when it is used as an iterator (monomorphization can elide it when only `contains` is used).

## Implementation Strategy

### Phase 1: Lexer and Parser

1. Add `DotDotLt` (`..<`) and `DotDotEq` (`..=`) tokens to the lexer
2. Add `Range` and `RangeInclusive` expression AST nodes
3. Parse range expressions at precedence level 14 (non-associative)
4. Update the operator precedence WEP to reflect `..<` and `..=` (replacing `..` and `..=`)

### Phase 2: Type Checking

1. Define `Range<T>` and `RangeInclusive<T>` as built-in generic structs in `core:prelude`
2. Infer `T` from the operands (both must have the same type after literal coercion)
3. Add `Step` trait and implement for integer types and `char`

### Phase 3: Iterator Integration

1. Implement `Iterator` for `Range<T: Step + Ord>` and `RangeInclusive<T: Step + Ord>`
2. Implement `IntoIterator` for both types
3. Verify for-of desugaring works with range expressions

### Phase 4: Methods and Traits

1. Implement `contains`, `is_empty` methods
2. Implement `Display`, `Inspect`, and `Eq` traits
3. Implement `IndexValue<Range<i32>>` and `IndexValue<RangeInclusive<i32>>` for `Array<T>`

## Consequences

### Positive

1. **Idiomatic counted loops**: `for let i of 0..<10` is clearer and more concise than C-style `for`
2. **Works with existing iterator system**: Ranges plug directly into `for-of`, `map`, `filter`, `fold`, etc.
3. **Type-safe membership testing**: `(0.0..<1.0).contains(&x)` works for any `Ord` type
4. **Array slicing sugar**: `arr[1..<5]` is more natural than `arr.slice(1, 5)`
5. **Minimal type count**: Two types cover the vast majority of use cases
6. **Explicit syntax**: Both `..<` and `..=` are self-documenting — no ambiguous "bare" `..`
7. **Generic**: Works with all integer types, `char`, and `Ord` types for membership testing
8. **Frees `..` for rest syntax**: `..` remains exclusively for struct rest patterns and destructuring, avoiding ambiguity

### Negative

1. **No partial ranges**: Users must write `arr.slice(2, arr.len())` instead of `arr[2..<]`
   - **Mitigation**: Can be added later without breaking changes
2. **No step_by**: Custom step sizes require C-style `for` loops
   - **Mitigation**: Can be added later as an iterator combinator
3. **No reverse iteration**: Requires C-style `for` or future `.rev()` combinator
   - **Mitigation**: Can be added later to the `Iterator` trait
4. **No range patterns in match**: Requires separate design work
   - **Mitigation**: Addressed in a future WEP
5. **`RangeInclusive` hidden `exhausted` flag**: Implementation detail leaks into codegen
   - **Mitigation**: Not visible to users; only affects compiler internals
6. **New `Step` trait**: Adds one more trait to the prelude
   - **Mitigation**: Small, focused trait with obvious purpose

## References

- [WEP: Operator Precedence and Associativity](./wep-2026-01-11-operator-precedence.md) — precedence level 14 (this WEP revises `..` to `..<`)
- [WEP: Iterator Traits Design](./wep-2026-01-24-iterator-traits.md) — iterator system that ranges integrate with
- [WEP: Indexing Traits Design](./wep-2026-01-20-indexing-traits.md) — `IndexValue` trait for range-based slicing
- [Rust `std::ops::Range` documentation](https://doc.rust-lang.org/std/ops/struct.Range.html)
- [Swift `Range` documentation](https://developer.apple.com/documentation/swift/range)

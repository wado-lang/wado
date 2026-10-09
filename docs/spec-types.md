# Types

This chapter covers the types a program declares values of: the primitives,
strings, tuples, lists, the never type, newtypes, structs, enums, variants and
flags, and how type arguments are inferred. Traits over these types are in
[Traits](./spec-traits.md).

## Prelude Types

These types, among others, come from the
[prelude](./spec-modules.md#the-prelude), so they need no `use`:

- `String`, UTF-8 text, and `StrSlice`, a view of part of one.
- `List<T>`, a growable sequence, and `Array<T>` and `Slice<T>`, the rest of
  [the sequence family](#the-sequence-family).
- `Option<T>` with its cases `Some(x)` and `None`, which `null` also denotes.
- `Result<T, E>` with its cases `Ok(x)` and `Err(e)`.
- `Stream<T>` and `Future<T>`, the Component Model's async stream and future.
- `i128` and `u128`, the 128-bit integers.

[`core:prelude`](./stdlib-core-prelude.md) lists them all.

## Primitive Types

Primitive types are built into the language (no import required):

<!-- {"fixture":"spec_types_primitives.wado"} -->

```wado
test {
    let signed: [i8, i16, i32, i64] = [-1, -2, -3, -4];
    let unsigned: [u8, u16, u32, u64] = [1, 2, 3, 4];
    let float: [f32, f64] = [0.5, 0.25];
    let half: [f16, bf16] = [0.5, 0.5];   // storage only, no arithmetic
    let flag: bool = true;
    let letter: char = 'a';
    assert signed.3 == -4 && unsigned.3 == 4 && float.1 == 0.25;
    assert half.0 == 0.5 && half.1 == 0.5 && flag && letter == 'a';
}
```

A primitive type has
[associated constants](./spec-functions.md#associated-constants) and static
methods, such as `i32::MAX` and `f64::sqrt`.
[`core:prelude`](./stdlib-core-prelude.md) lists them.

### Half Precision (`f16`, `bf16`)

`f16` is IEEE 754 binary16. `bf16` is bfloat16: `f32`'s exponent range with an
8-bit significand. Both are storage types. They hold a value and do no
arithmetic:

- No arithmetic or bitwise operator applies to either, unary `-` included.
- `as` converts them as it does any float (see
  [Numeric Casts](./spec-expressions.md#numeric-casts)).
- A numeric literal coerces to either type and is rounded once (see
  [Floating-Point Literals](./spec-literals.md#floating-point-literals)).
- A comparison answers as it would on both operands widened to `f32` (see
  [Float Comparison](./spec-standard-traits.md#float-comparison)).
- Neither type crosses a component boundary (see
  [Type Mapping](./spec-components.md#type-mapping-at-component-boundaries)).

Values convert through methods and the conversion traits:

<!-- {"fixture":"spec_types_half_precision.wado"} -->

```wado
let one = f16::from_bits(0x3C00);   // 1.0
assert one.to_bits() == 0x3C00;
let wide = f32::from(one);          // exact; `f64::from` too
assert wide == 1.0;
let near = f16::from_f32(0.1);      // rounds to the nearest f16
assert near.to_bits() == 0x2E66;
assert f16::try_from(wide).unwrap() == one;   // 1.0 survives the round trip
```

- `to_bits` and `from_bits` read and write the 16 bits as a `u16`.
- `From<f16>` and `From<bf16>` are implemented for `f32` and `f64`. Widening is
  exact.
- `from_f32` and `from_f64` narrow. They round once, to nearest with ties to
  even. A value that rounds past the largest finite one becomes an infinity.
- `TryFrom<f32>` and `TryFrom<f64>` answer `Ok` only where the value survives the
  round trip, a NaN included. Otherwise they answer `Err(ConvertError)`.
- `from_str` rounds the decimal text once, as a literal is rounded. Text that
  rounds past the largest finite value parses to an infinity.
  `from_str_lenient` takes the spellings `LenientFromStr` accepts.
- No conversion trait links `f16` and `bf16`, because each keeps something the
  other drops: `f16` has the shorter exponent range, `bf16` the shorter
  significand. `as` converts between them, rounding to nearest.

Both types carry `f32`'s associated constants under the same names (`MAX`,
`MIN`, `MIN_POSITIVE`, `EPSILON`, `INFINITY`, `NAN`, `MANTISSA_DIGITS`, …) and
`is_nan`.

A template renders either type through its `f32` value, as
[Display Output](./spec-literals.md#display-output) and
[Inspect Output](./spec-literals.md#inspect-output) state.
[Serialization](./spec-serialization.md) states how either type is written and
read.

Rationale: [WEP: Half-Precision Primitives](./wep-2026-09-22-half-precision-primitives.md).

## 128-bit Integer Types (i128/u128)

`i128` and `u128` are prelude types, not primitives. Literals, operators and
casts treat them as integers all the same:

<!-- {"fixture":"spec_types_wide_int.wado"} -->

```wado
let a: u128 = 42;                      // literal coercion
let b = u128::from_u64(1_000_000);     // explicit construction
assert a + b == 1_000_042;             // via Add
assert a < b;                          // via Ord

// the low and high 64-bit halves
assert a.low() == 42;
assert a.high() == 0;
```

Available operations:

| Category   | Operations                                                                   |
| ---------- | ---------------------------------------------------------------------------- |
| Arithmetic | `+`, `-`, `*`, `/`, `%`, unary `-` (i128)                                    |
| Comparison | `==`, `!=`, `<`, `<=`, `>`, `>=`                                             |
| Bitwise    | `&`, `\|`, `^`, `~`, `<<`, `>>`                                              |
| Conversion | `from_u64()`, `from_i64()`, `from_f64()`, `low()`, `high()`, `as`, `TryFrom` |

Literal and range patterns work on them in every pattern position, nested ones
included: `match [x, y] { [1..=5, _] => … }`. Each pattern matches exactly when
the equivalent `==` or range comparison holds.

`as` casts them as it does any integer (see
[Numeric Casts](./spec-expressions.md#numeric-casts)):

<!-- {"fixture":"spec_types_wide_int.wado"} -->

```wado
let a = 42 as u128;                    // numeric → wide int
assert 1.0e40 as u128 == u128::MAX;    // float → wide int saturates
assert 'A' as i128 == 65;              // bool, char → wide int, as to any integer
assert a as f64 == 42.0;               // wide int → float, rounded to nearest, ties to even
assert (a + 256) as u8 == 42;          // wide int → int truncates
assert (-1 as i128) as u128 == u128::MAX;  // i128 ↔ u128 transmutes
```

`TryFrom` gives the checked conversions, such as `i64::try_from(a)` and
`u128::try_from(n)`. Each answers `Err` for a value out of the target's range.

## SIMD Types (v128)

Wado exposes WebAssembly SIMD via the `core:simd` module. A single primitive type `v128` represents a 128-bit vector, and 10 newtypes over it give it type-safe lane interpretations:

| Category | Types                              |
| -------- | ---------------------------------- |
| Signed   | `i8x16`, `i16x8`, `i32x4`, `i64x2` |
| Unsigned | `u8x16`, `u16x8`, `u32x4`, `u64x2` |
| Float    | `f32x4`, `f64x2`                   |

All SIMD newtypes share the `v128` base and can be reinterpreted via `as` cast (zero-cost). Each type provides `splat()` construction, lane access, per-lane comparison methods such as `eq` and `lt`, and the operators its lanes support. The sets differ by type, following the Wasm SIMD instruction set:

- `/` exists only on `f32x4` and `f64x2`, and the bitwise operators only on the integer types.
- `i8x16` and `u8x16` have no `*`.
- `u64x2` compares only with `eq` and `ne`.
- `i8x16` and `i16x8` read a lane with `extract_lane_s` or `extract_lane_u`; the other types use `extract_lane`.

Sequence literal coercion is supported via `impl From<Array<i32>> for i32x4`:

<!-- {"fixture":"spec_types_simd.wado"} -->

```wado
let v: i32x4 = [1, 2, 3, 4];     // sequence literal coercion
let w = i32x4::splat(10);        // broadcast
let sum = v + w;
assert sum.extract_lane(0) == 11 && sum.extract_lane(3) == 14;
let mask = v.lt(&w);             // per-lane comparison mask
assert mask.extract_lane(0) == -1;
```

`v128` itself takes no arithmetic, bitwise or shift operator. The lane types
carry those, and each works lane by lane. A shift takes one `u32` count and
shifts every lane by it.

The comparison methods (`eq`, `ne`, `lt`, `le`, `gt`, `ge`) answer a mask:
each lane is all ones where the comparison holds and all zeros where it does
not. On an integer lane type the mask has that type. On `f32x4` it is an
`i32x4`, and on `f64x2` an `i64x2`. `bitselect` takes such a mask.

`==` and `!=` are not lane-wise. On `v128` and on every lane type, they compare
all 128 bits and answer one `bool`, because every lane type is a newtype over
the same `v128`. So a NaN lane equals itself, and a `0.0` lane differs from a
`-0.0` lane. Use `eq` for the IEEE comparison.

<!-- {"fixture":"spec_types_simd.wado"} -->

```wado
let nan = f32x4::splat(f32::NAN);
assert nan == nan;                  // the bits are equal
let mask = nan.eq(&nan);            // an i32x4: IEEE says NaN != NaN
assert mask.extract_lane(0) == 0;
```

Beyond basic arithmetic and comparison, types provide specialized operations: saturating arithmetic (`add_sat_s/u` and `sub_sat_s/u` on signed types, `add_sat` and `sub_sat` on unsigned ones), lane narrowing/extension, extended multiplication, pairwise addition, type conversion between integer and float, and bit selection. Where an input is out of range, the strict operations are defined: `trunc_sat_*` saturates, and `swizzle` gives a zero lane for an index past the last lane. See [`core:simd`](./stdlib-core-simd.md) for the full API.

### Relaxed SIMD

Relaxed SIMD operations trade strict determinism for performance. Edge-case behavior (NaN, out-of-range values) is [host-defined](./spec-overview.md#behavior-classes), and fixed on one host. Methods use the `relaxed_` prefix on existing newtypes:

- Fused multiply-add: `f32x4/f64x2.relaxed_madd(b, c)`, `relaxed_nmadd(b, c)`
- Min/Max: `f32x4/f64x2.relaxed_min/max` (faster than strict `min`/`max`)
- Truncation: `i32x4::relaxed_trunc_f32x4_s/u`, `relaxed_trunc_f64x2_s/u_zero`
- Lane select: `relaxed_laneselect` on `i8x16`, `i16x8`, `i32x4`, `i64x2`
- Swizzle: `i8x16.relaxed_swizzle`
- Dot product: `i16x8.relaxed_dot_i8x16_i7x16_s`, `i32x4::relaxed_dot_i8x16_i7x16_add_s(a, b, &c)`
- Q15 multiply: `i16x8.relaxed_q15mulr_s`

A relaxed method is an ordinary method. Calling one declares no effect.

Rationale: [WEP: SIMD v128 Types](./wep-2026-01-31-simd-v128.md).

## String Type

`String` is text: a sequence of Unicode scalar values, stored as UTF-8. Every
`String` is valid UTF-8, so it crosses a component boundary as a Component
Model `string` unchanged.

A `String` is a value like any other
([Value Semantics](./spec-memory.md#value-semantics)). `push_str` changes the
receiver in place and `+=` reassigns the binding, and neither reaches a copy
the caller holds.

### Index Access (Prohibited)

Direct index access is prohibited to avoid ambiguity between byte and character indexing:

<!-- {"fixture":"spec_types_string_index.wado"} -->

```wado
let s = "Hello世界";
let first = s[0];
```

A range index is prohibited too:

<!-- {"fixture":"spec_types_string_slice_index.wado"} -->

```wado
let s = "Hello世界";
let head = s[0..<5];
```

Use explicit methods instead:

<!-- {"fixture":"spec_types_string.wado"} -->

```wado
let s = "Hello世界";

// byte-level access
let bytes: List<u8> = s.bytes().collect();
assert bytes[0] == b'H';

// character-level access
let chars: List<char> = s.chars().collect();
assert chars[5] == '世';

assert s.len() == 11;       // in bytes
assert !s.is_empty();
```

### Iterating Bytes and Characters

`bytes()` and `chars()` return iterators, so `for-of` walks them directly:

<!-- {"fixture":"spec_types_string.wado"} -->

```wado
let mut out = "";
for let c of "hello".chars() {
    out.push(c);
}
assert out == "hello";

let mut sum = 0;
for let b of "hello".bytes() {
    sum += b as i32;         // 104, 101, 108, 108, 111
}
assert sum == 532;
```

### String Building

`push_str` appends to a `String` in place:

<!-- {"fixture":"spec_types_string.wado"} -->

```wado
let mut builder = String::with_capacity(20);
builder.push_str("Hello");
builder.push_str(", ");
builder.push_str("World!");
assert builder == "Hello, World!";

// `join` concatenates a list with a separator
let parts: List<String> = ["a", "b", "c"];
assert parts.join(",") == "a,b,c";
```

### Concatenation

#### New String (`+` operator)

<!-- {"fixture":"spec_types_string.wado"} -->

```wado
let s1 = "hello";
let s2 = " world";
let s3 = s1 + s2;           // a new String
assert s3 == "hello world";
assert s1 == "hello";
```

#### Reassignment (`+=` operator)

`a += b` is `a = a + b` through `Add`. `String` follows the same rule as every other type:

<!-- {"fixture":"spec_types_string.wado"} -->

```wado
let mut s = "hello";
s += " world";              // s = s + " world"
s += "!";
assert s == "hello world!";
```

Rationale: [WEP: String Type Design](./wep-2026-01-15-string-type-design.md).

### String Views

`StrSlice` is a prelude type: a view of part of a string, its ends on UTF-8
character boundaries. Creating one copies nothing, and `to_string()` is where a
copy is made. It is an ordinary library type; nothing in the language treats
the name specially.

<!-- {"fixture":"spec_traits_str_slice.wado"} -->

```wado
let v = "banana".as_str_slice();
let part = v.slice(1, 4);            // panics off a character boundary
assert part == "ana";
assert part.len() == 3;              // in bytes
assert part.to_string() == "ana";    // copies out, here and only here
```

`slice(start, end)` takes byte offsets from the view's start. It traps when the
range is out of bounds or either end is not a character boundary.
`slice_unchecked` leaves both checks to the caller. Calling it with a range
that fails either check is a
[contract violation](./spec-overview.md#behavior-classes), and the behavior is
unconstrained: the view breaks its own invariant, and it may read bytes outside
the view it was taken from. A view refers to its
string's bytes as a [slice](#slice-semantics) does, with the same snapshot and
aliasing behavior.

`StrSlice` carries the search and split methods (`contains`, `starts_with`,
`find`, `split`, `split_once`, the trims, the `strip_*` family), and `String`'s
own delegate to it. A method that answers with part of its input returns a view
of it, so working on part of a string does not copy it out. `StrSlice` and
`String` compare in either order, and two views compare and order by their
text.

A signature takes a `String`, a reference to one, or a view of one alike through
[`AsStrSlice`](./spec-standard-traits.md#asstrslice).

Rationale: [WEP: String Views](./wep-2026-09-13-string-slice.md).

## Tuples

Tuple types use bracket syntax `[T1, T2, ...]`. The literal that builds one is
in [Tuple Literals](./spec-literals.md#tuple-literals).

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
let point: [i32, i32] = [10, 20];
let record: [String, i32, bool] = ["Alice", 30, true];
assert point.1 == 20 && record.0 == "Alice";
```

### Tuple Element Access

Tuple elements are accessed by constant index using dot notation or bracket notation:

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
let t = [10, "hello", true];
let x = t.0;      // dot notation
let y = t[1];     // bracket notation
let z = t.2;
assert x == 10 && y == "hello" && z;
```

A variable index is not allowed:

<!-- {"fixture":"spec_literals_tuple_index.wado"} -->

```wado
let i = 1;
let w = t[i];     // Error: tuple index must be a constant integer
```

### Unit vs Empty Tuple

The unit type `()` and empty tuple `[]` are distinct:

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
let unit: () = ();    // Unit type/value
let empty: [] = [];   // Empty tuple (rarely used)
assert `${unit:?}` == "()" && `${empty:?}` == "[]";
```

They take separate `impl`s, so a method defined on one is not found on the other.

`[]` cannot cross a component boundary. It has no Component Model representation:
a `tuple` carries at least one type, and `()` is the type that carries none. An
export naming one is rejected at compile time.

## Lists

`List<T>` is a growable sequence of `T`. A bracket literal builds one where a
`List` is expected ([List Literals](./spec-literals.md#list-literals)), and its
constructors build one from a size.

### List Constructors

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
let arr = List::<i32>::with_capacity(10);     // empty list with room for 10 elements
let bools = List::<bool>::filled(100, true);  // list of 100 elements, all true
assert arr.len() == 0 && bools.len() == 100 && bools[99];
```

### List Operations

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
let mut arr: List<i32> = [1, 2, 3];

// Index access (read)
let first = arr[0];
assert first == 1;

// Index assignment (write)
arr[0] = 100;        // Requires a `let mut` binding
arr[1] = 200;

// List methods
arr.push(4);         // Add element to end
let len = arr.len(); // Get length
assert arr == [100, 200, 3, 4] && len == 4;
```

An index outside the list traps ([Bounds Checks](#bounds-checks)).

### Sorting

Every sort is stable and O(n log n) in the worst case:

| Method        | Mutates? | Comparator                          |
| ------------- | -------- | ----------------------------------- |
| `sort()`      | Yes      | `Ord::cmp` (requires `T: Ord`)      |
| `sort_by()`   | Yes      | Custom `fn mut(&T, &T) -> Ordering` |
| `sorted()`    | No       | `Ord::cmp` (requires `T: Ord`)      |
| `sorted_by()` | No       | Custom `fn mut(&T, &T) -> Ordering` |

`sort()` and `sorted()` put every NaN in a list of floats last. See
[Float Comparison](./spec-standard-traits.md#float-comparison).

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
let mut nums: List<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending
assert nums == [1, 3, 5, 8];

let orig: List<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns a new sorted list
assert asc == [1, 3, 5, 8] && orig == [5, 3, 8, 1];
```

## The Sequence Family

Three prelude types hold contiguous sequences:

|                | Fixed length | Growable  |
| -------------- | ------------ | --------- |
| Owned          | `Array<T>`   | `List<T>` |
| Reference view | `Slice<T>`   | —         |

`Slice<T>` is the read-only vocabulary type. An algorithm that only reads is
written once against a slice, and the owned types reach it through
`as_slice()`. The methods the three share come from the
[`Sequence` and `AsSlice`](./spec-standard-traits.md#sequence-and-asslice)
traits.

A conversion's name says what it costs: `as_*` returns a view of the same
elements, and `to_*` copies them.

| From → To                  | Method       | Elements copied       |
| -------------------------- | ------------ | --------------------- |
| `Array` / `List` → `Slice` | `as_slice()` | none                  |
| `Slice` → `Array` / `List` | `to_*()`     | all                   |
| `List` → `Array`           | `to_array()` | all, sized to `len()` |

Indexing by a range of `i32` yields a `Slice<T>` of the elements the range
covers, as `slice(start, end)` does: `xs[1..<4]` holds `xs[1]`, `xs[2]` and
`xs[3]`, as does `xs[1..=3]`. Both clamp the range to the sequence.

### Slice Semantics

A slice refers to the whole backing array plus a start and an end. It is an
ordinary value: assigning one copies those three fields and never the elements.
A view reads its elements but never hands out `&mut` into the array, so nothing
writes through it (see [Dispatch](./spec-standard-traits.md#dispatch)). Two
consequences follow, both memory-safe:

- Snapshot. A view keeps referring to the buffer it was created from, so a
  source `List` that grows and reallocates is not observed.
- Aliasing. A write to the source that does not reallocate is visible through
  the view.

A source `List` that shrinks (`truncate`, `clear`, `pop`, `remove`) may release
the slots past its new length, so the collector can reclaim what they held.
Reading a released slot through a view, or through an iterator over the list,
is a [contract violation](./spec-overview.md#behavior-classes). The result is
unspecified: some value of the element type, or a trap. That value need not be
the one the slot held; an `Option` element may read as `None`.

A slice compares, orders, displays, and inspects by its elements, not by the
buffer it refers to.

### Bounds Checks

`get(i)` returns `Option<T>` on all three types. `xs[i]` traps when `i` is
outside `0..<len()`. On a slice that includes a negative index, even where the
backing array holds an element, since a view never reads outside itself. The
trap is the whole contract: its message may be any text, and which text is
[unspecified](./spec-overview.md#behavior-classes).

`get_unchecked(i)` leaves the check to the caller, who must guarantee
`0 <= i < len()`. Calling it with any other `i` is a
[contract violation](./spec-overview.md#behavior-classes), and the result is
unspecified: an element of the backing array, or a trap. Either way the result
is a valid value of the element type, because every element read is
bounds-checked by the Wasm engine.

Rationale: [WEP: The Sequence Family](./wep-2026-06-02-sequence-family.md).

## The Never Type `!`

The never type `!` is the bottom type: it is a subtype of every type. An
expression of type `!` never produces a value. `panic()` and `unreachable()`
both return `!`.

So an expression of type `!` may stand in any value position without a type
mismatch:

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
// In a match arm: the None branch panics, so the match has type i32
let opt: Option<i32> = Option::<i32>::Some(5);
let x = match opt {
    Some(v) => v,
    None => panic("unexpected none"),
};
assert x == 5;

// In a let binding with explicit type annotation
let y: i32 = panic("unreachable");

// In a binary expression: execution diverges before the addition
let z: i32 = panic("boom") + 1;
```

The `!` type can be written explicitly as a return type:

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
fn fail(msg: String) -> ! {
    panic(msg);
}

test {
    let n: i32 = if true { 1 } else { fail("unreachable") };
    assert n == 1;
}
```

## Newtype

`type T = U` creates a newtype: a distinct type with the same values as its base type.

<!-- {"fixture":"spec_types_newtype.wado"} -->

```wado
type Meters = f64;

let m: Meters = 1000.0;       // literal coercion
let sum = m + m;              // Meters + Meters -> Meters
let raw: f64 = sum as f64;    // explicit cast required
assert raw == 2000.0;
```

Two newtypes over one base do not mix:

<!-- {"fixture":"spec_types_newtype_mix.wado"} -->

```wado
type Meters = f64;
type Kilometers = f64;

let m: Meters = 1000.0;
let km: Kilometers = 1.0;
let bad = m + km;
```

### Properties

- `T` is a distinct type from `U` (no implicit conversion)
- `T` inherits all methods, operators, and traits from `U`
- Explicit `as` cast required to convert between `T` and `U`
- Zero runtime cost
- Literal coercion to `T` when type context expects `T`

A newtype does not carry an invariant of its own. `as` converts in both
directions at no cost, so `T` admits exactly what `U` admits. An invariant the
base type does not enforce, such as UTF-8 in a byte view, belongs in a `struct`
with a private field and a checked constructor, where the check is the only way
in. So does a type that must hide its base type's methods:

<!-- {"fixture":"spec_types_checked_struct.wado"} -->

```wado
pub struct Percent {
    value: i32,
}

impl Percent {
    pub fn new(value: i32) -> Option<Percent> {
        if value < 0 || value > 100 {
            return null;
        }
        return Option::Some(Percent { value });
    }
}

test {
    assert Percent::new(50) matches { Some(_) };
    assert Percent::new(150) matches { None };
}
```

### Method Signature Substitution

When calling inherited methods on a newtype, parameters and return types are substituted:

<!-- {"fixture":"spec_types_newtype_methods.wado"} -->

```wado
struct Point {
    x: i32,
    y: i32,
}

impl Point {
    fn distance(&self, other: &Point) -> f64 {
        let dx = (other.x - self.x) as f64;
        let dy = (other.y - self.y) as f64;
        return f64::sqrt(dx * dx + dy * dy);
    }
}

type Location = Point;

test {
    let loc1: Location = Point { x: 0, y: 0 } as Location;
    let loc2: Location = Point { x: 3, y: 4 } as Location;
    assert loc1.distance(&loc2) == 5.0;   // the parameter expects &Location
}
```

Every occurrence of the base type in the signature is substituted: the
receiver, each parameter, and the return type, including inside a generic
argument. A method returning `Option<Point>` or `List<Point>` returns
`Option<Location>` or `List<Location>` on a `Location`. An argument of the base
type where the substituted signature expects the newtype is a type mismatch:
`loc1.distance(&Point { x: 0, y: 0 })` is an error.

### Inherited Associated Functions and Traits

A newtype inherits its base type's associated functions too. A function whose
return type is the base type returns the newtype, and the base's type arguments
settle the call, so it needs no turbofish:

<!-- {"fixture":"spec_types_newtype.wado"} -->

```wado
type Octets = List<u8>;

let mut b = Octets::with_capacity(16);   // b: Octets
b.push(0xff);
assert b.len() == 1 && b[0] == 0xff;
```

A newtype implements every trait its base implements. It satisfies a bound
the base satisfies (`fn max<T: Ord>(a: T, b: T)` takes two `Meters`). `for-of`
iterates it as it iterates the base, by value and through `&`, and `collect()`
builds it wherever it builds the base (`let b: ByteList = s.bytes().collect()`).

### Newtype-Specific Methods

<!-- {"fixture":"spec_types_newtype_methods.wado"} -->

```wado
impl Location {
    fn name(&self) -> String {            // only on Location, not Point
        return `(${self.x}, ${self.y})`;
    }
}

test {
    let here = Point { x: 3, y: 4 } as Location;
    assert here.name() == "(3, 4)";
}
```

A trait impl written for the newtype wins over the one it inherits from the
base. See [The Order](./spec-traits.md#the-order).

### A Newtype over a Reference

A newtype whose base is a reference (`type H = &mut T`) is called as that
reference is. A trait method is looked up on `H`, then on `&mut T`, then on `T`,
and the first that answers is called. An impl written for `&mut T` is the
newtype's base's, inherited as any newtype inherits from its base. The inherent
methods are `T`'s.

A method of `T` is the reference's own call, so nothing in its signature is
substituted with the newtype. A method of `T` returning `T` returns a `T`, a
copy, not an `H` onto the same place. A method taking `&mut self` needs every
reference on the way to `T` to be `&mut`: through `type V = &T`, or through
`&H`, it is an error, however the binding is declared.

<!-- {"fixture":"newtype_over_mut_ref_inherits_trait_methods.wado"} -->

```wado
type Handle = &mut Counter;

impl Name for Handle {
    fn name(&self) -> String {
        return "handle";
    }
}

type Plain = &mut Counter;

export fn run() with Stdout {
    let mut c = Counter { n: 1 };
    let d = Counter { n: 2 };
    let h = &mut c as Handle;
    assert h.bump(&d) == 3 && c.n == 3;   // writes through to `c`
    let mut copy = h.snap();              // a `Counter`, not a `Handle`
    copy.n = 100;
    assert c.n == 3;

    let mut e = Counter { n: 0 };
    assert (&mut c as Handle).name() == "handle";   // its own impl first
    assert (&mut e as Plain).name() == "counter";   // else `Counter`'s
    println("ok");
}
```

### Casts

`as` converts between a newtype and its base in both directions, between two
newtypes over the same base, and through a chain of newtypes in one step. A
generic newtype casts to and from its base instantiation.

A cast to a reference takes a reference and converts its referent the same way:
`&Meters as &f64`. `&mut T` may narrow to `&T`, never the reverse. A cast to
anything else reads through its operand's references and converts what they
point at, so `(&x) as i64` converts `x`.

<!-- {"fixture":"spec_types_newtype.wado"} -->

```wado
type A = i32;
type B = A;
type C = B;

let c: C = 1;
let a = c as A;    // one cast through the chain
let i = c as i32;  // straight to the ultimate base
assert i == 1 && a as i32 == 1;

type Wrapper<T> = List<T>;
let w = [1] as Wrapper<i32>;
let l = w as List<i32>;
assert l.len() == 1;
```

`as` does not reach a newtype nested inside another type: `List<Meters>` does
not cast to `List<f64>`, and `Option<Meters>` does not cast to `Option<f64>`.

A function type is the exception, since a function value is reused unchanged.
`as` converts one function type to another when both take the same number of
parameters and each parameter and the return type differ only by newtype steps,
at the top, under a reference, or inside a function type that is otherwise
identical. The cast may also widen `fn` to `fn mut` and add effects, never the
reverse. A function type casts to no other kind of type, and a type parameter
is not known to be a function type, so `f as T` is an error.

<!-- {"fixture":"spec_types_newtype.wado"} -->

```wado
type Meters = f64;
let double = |x: f64| x * 2.0;

let h = double as fn(Meters) -> Meters;
assert h(1.5) as f64 == 3.0;
let mut r = double as fn mut(f64) -> f64;   // `fn` widens to `fn mut`
assert r(1.0) == 2.0;
```

A cast between unrelated parameter types is an error:

<!-- {"fixture":"cast_fn_type_unrelated.wado"} -->

```wado
let h = double as fn(i32) -> f64;
```

Rationale: [WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md).

## Structs

Wado uses `struct` for structured data types. A struct crosses a component boundary as a Component Model `record`.

<!-- {"fixture":"spec_types_structs.wado"} -->

```wado
struct User {
    name: String,
    age: i32,
    active: bool,
}

// A struct may hold itself through an Option
struct Node {
    value: i32,
    next: Option<Node>,
}

test {
    let list = Node { value: 1, next: Option::Some(Node { value: 2, next: null }) };
    assert list.next.unwrap().value == 2;
}
```

In a struct's field types, `Self` names the struct being declared, so
`next: Option<Self>` says what `next: Option<Node>` says. This holds for a
struct declared in a function body too. Inside an `impl` or `trait` method, a
local struct's `Self` is that struct, not the type the `impl` is for.

### Field Visibility

A struct field takes the visibility modifiers other declarations take, and
reaches as far as [Visibility](./spec-modules.md#visibility) states.

<!-- {"fixture":"spec_types_field_visibility.wado"} -->

```wado
pub struct Config {
    pub name: String,   // visible to other packages
    internal tag: i32,  // visible to other files in this package
    secret: i32,        // private to this file
}

test {
    let c = Config { name: "app", tag: 1, secret: 2 };
    assert c.secret == 2;
}
```

Reading, setting, or binding a field beyond its reach is a compile error. This
holds for field access (`c.secret`), a struct literal (`Config { secret: ... }`),
and a destructuring pattern (`let Config { secret, .. } = c`, `match`).

A literal in another module may still omit a field it cannot reach when the
field has a default expression (`f: T = expr`). The default resolves in the
defining module (see [Struct Field Defaults](#struct-field-defaults)), so the
field is never read or set across the boundary. A field out of reach with no
default cannot be filled from another module, so only a function within reach
can construct such a struct.

### Struct Construction

<!-- {"fixture":"spec_types_structs.wado"} -->

```wado
let user = User { name: "Alice", age: 30, active: true };

// Shorthand (variable name matches field)
let name = "Bob";
let age = 25;
let bob: User = { name, age, active: false };
assert bob.name == "Bob";

// Implicit struct literal (requires type annotation)
let carol: User = { name: "Carol", age: 30, active: true };
assert carol.age == user.age;
```

A literal's field values are evaluated once each, in the order the source
writes them, not the order the struct declares them.
`P { y: f(), x: g() }` calls `f` before `g`.

A leading `..base` is a functional update. It fills every field the literal
does not list from the struct value `base`. The listed fields override it, and
`base` itself is left unchanged.

<!-- {"fixture":"spec_types_structs.wado"} -->

```wado
let u2 = User { ..user, age: 31 };  // every field from `user`, age replaced
assert u2.name == "Alice" && u2.age == 31;
```

The spread is leading and single: a field written before it would be overwritten
and unused, so `User { age: 31, ..user }`, a second spread, and a bare
`User { ..user }` (a plain copy) are all errors. A `..base` cannot read a field
that is not reachable at the use site, so it never exposes a private field across
a module boundary.

`base` must have the literal's own type, which an implicit literal takes from
its expected type (`let u: User = { ..user, age: 31 }`). Its type arguments take
part in inference like an explicit field. An override cannot change one, so with
`b: Box<i32>`, `Box { ..b, value: "s" }` is a type mismatch. The members are
evaluated once each, in source order, so `base` comes first. An override may
repeat the value `base` already holds.

Rationale: [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Anonymous Structs

A `{ … }` literal where no struct or map type is expected builds an anonymous
struct. It names no declaration: its type is its shape, the field names and
each field's type. Two literals of one shape have one type, so
`if c { { x: 1, y: 2 } } else { { x: 3, y: 4 } }` type-checks. Its fields are
read, destructured and nested as a named struct's are.

<!-- {"fixture":"spec_types_structs.wado"} -->

```wado
let p = { x: 1, y: 2 };
let line = { start: p, end: { x: 10, y: 20 } };
let { start: { x, y }, end } = line;
assert x == 1 && y == 2 && end.x == 10;
```

No `impl` can name an anonymous struct, so it takes derived traits
([Derivation Policy](./spec-traits.md#derivation-policy)) and blanket impls
([Candidates](./spec-traits.md#candidates)) only.

#### Composition

An anonymous literal may compose spread bases: `{ ..a, ..b, field: v }`
builds an anonymous struct whose fields are the union of the bases' and explicit
fields. Members apply in source order, and the last contributor of a name wins,
its type included. Every member is evaluated once, in source order. Each base is
a struct value, and a spread of anything else is an error.

Unlike a named struct's leading-single `..base`, composition allows spreads in
any position and more than one. One rule limits them: a member every one of
whose fields is overwritten by a later member is a dead-write error, so
`{ ..a, ..b }` with `a` and `b` of one struct type is rejected. A lone `{ ..a }`
is rejected too, since under value semantics it only copies `a`. A spread reads
only the fields reachable at the use site, as `a.f` would.

<!-- {"fixture":"spec_types_structs.wado"} -->

```wado
let base = { user_id: 1, ip: "10.0.0.1" };
let event = { ..base, level: "warn" };  // { user_id, ip, level }
assert event.user_id == 1 && event.level == "warn";
```

Composition applies where no nominal type is expected. An expected struct type
makes the literal a named one, with the named struct's rule
([Struct Construction](#struct-construction)), and an expected map
type makes it a key-value literal ([`..base` Spread](./spec-literals.md#base-spread)).
A key-value spread with no map type expected is an error.

Rationale: [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

### Struct Destructuring

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
let p = Point { x: 10, y: 20 };
let { x, y } = p;                  // the type comes from `p`
assert x == 10 && y == 20;
```

Naming the type checks it, and a field may bind under another name:

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
let Point { x, y } = p;            // the type is written, and checked
assert x + y == 30;

let { x: horizontal, y: vertical } = p;
assert horizontal == 10 && vertical == 20;
```

A `mut` binds every field mutably, as a copy:

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
let mut { x, y } = p;
x += 1;
assert x == 11 && p.x == 10;
```

`..` ignores the remaining fields:

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
struct Person { name: String, age: i32, email: String }
let person = Person { name: "Ann", age: 40, email: "ann@example.com" };
let { name, .. } = person;
assert name == "Ann";
```

A field pattern nests:

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
struct Line { start: Point, end: Point }
let line = Line { start: Point { x: 0, y: 1 }, end: Point { x: 2, y: 3 } };
let { start: { x: x1, y: y1 }, end: { x: x2, y: y2 } } = line;
assert x1 + y1 + x2 + y2 == 6;
```

`for-of` destructures each element:

<!-- {"fixture":"spec_types_destructuring.wado"} -->

```wado
let points: List<Point> = [Point { x: 1, y: 2 }, Point { x: 3, y: 4 }];
let mut sum = 0;
for let { x, y } of points {
    sum += x * y;
}
assert sum == 14;
```

### Struct Field Defaults

Struct fields may declare a default expression with `= expr`. Fields with defaults may be omitted at construction sites; fields without defaults are required:

<!-- {"fixture":"spec_types_field_defaults.wado"} -->

```wado
struct ServerConfig {
    host: String,            // required
    port: i32 = 8080,        // optional
    timeout: i32 = 30,       // optional
    debug: bool = false,     // optional
}

test "omitted fields take their defaults" {
    let c = ServerConfig { host: "localhost" };
    assert c.port == 8080 && c.timeout == 30 && !c.debug;

    let d = ServerConfig { host: "localhost", port: 3000 };
    assert d.port == 3000 && d.timeout == 30;
}
```

A literal that leaves out a field with no default is an error:

<!-- {"fixture":"spec_types_field_default_missing.wado"} -->

```wado
let c = ServerConfig { port: 3000 };
```

A default expression is evaluated each time a literal omits its field. It must
perform no effect, and it cannot name another field. A literal may omit every
field that has a default, down to `ServerConfig {}` where all of them do.
Destructuring sees every field, defaulted or not.

A default resolves its names in the module that declares the struct, not where
the literal is written, as a function's default does
([Where a Default Resolves](./spec-functions.md#where-a-default-resolves)).

A default may name the struct's own type parameters. Each literal settles them
first, from a turbofish, its annotation, or the fields it lists, and the default
is evaluated at those types:

<!-- {"fixture":"spec_types_field_defaults.wado"} -->

```wado
struct Holder<T: Default> { a: T, b: T = T::default() }

test "a default at the literal's type arguments" {
    let h: Holder<String> = { a: "x" };
    assert h.b == "";
    let s = Holder { a: 5 };              // T = i32 from `a`
    assert s.b == 0;
}
```

A field with a default is optional in deserialization too (see
[Missing, Repeated, and Unknown Fields](./spec-serialization.md#missing-repeated-and-unknown-fields)).

Whether a struct derives `Default` from its field defaults is stated in
[Auto-Derivation](./spec-standard-traits.md#auto-derivation).

Rationale: [WEP: Default Arguments](./wep-2026-04-11-default-arguments.md).

## Generic Type Inference

Wado infers type arguments for struct literals, variant constructors, and generic function and method calls. It uses two complementary mechanisms.

Forward inference derives type parameters from the values provided (fields, payloads, or arguments):

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
struct Box<T> { value: T }

test "forward" {
    let b = Box { value: 42 };              // Box<i32>: T = i32 from the field
    let opt = Option::Some("hello");        // Option<String>: T = String from the payload
    let check: [Box<i32>, Option<String>] = [b, opt];
    assert check.0.value == 42;
}
```

Backward inference derives type parameters from an expected type context (variable annotation, function parameter type, or return type):

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
let none: Option<i32> = Option::None;   // T = i32 from the annotation
let ok: Result<i32, String> = Result::Ok(42);
// T = i32 from the payload (forward), E = String from the annotation (backward)
assert none == null && ok.unwrap() == 42;
```

When both mechanisms apply, they must agree. An untyped literal takes its type from the expected type, so `let x: Option<i64> = Option::Some(42)` is an `Option<i64>`. A value whose type is already fixed must match it: with `y: i32`, `let b: Box<i64> = Box { value: y }` is a type mismatch. Backward inference fills in any parameter the values do not mention.

In a generic call, a numeric literal answers last wherever it stands in an
argument, a nested generic call or constructor included. It takes the type the
call's other arguments settle, so with `fn pick<T>(b: Box<T>, fallback: T)` and
`x: u64`, `pick(Box { value: 1 }, x)` is `pick::<u64>`, as it would be with
`&Box { value: 1 }`, with `wrap(1)` for a generic `wrap` returning `Box<U>`, or
with the arguments swapped. Where no argument settles a parameter, the type
expected of the call does: `let y: u64 = pick(Box { value: 1 }, 2)` is
`pick::<u64>`. Only a parameter neither settles takes a default from its
literals: `f64` if one of them is a float, else `u8` if every one is a byte
literal, else `i32`.

The type expected of a call reaches its arguments first, as a hint. A literal
whose type comes from context, a sequence literal (`[]`, `[1]`) or a struct
literal without a name, is checked against its parameter with the hint
substituted, so `let l: List<i32> = identity([])` is `identity::<List<i32>>`
rather than a mismatch with the empty tuple. Any other argument is checked
against the parameter alone, so a typed argument still decides what the hint
only suggests.

<!-- {"fixture":"infer_expected_type_hints_context_literal.wado"} -->

```wado
let l: List<i32> = identity([]);
assert l.is_empty();
```

Arithmetic on literals answers as a literal does (`Box { value: 1 << 32 }`), and
so does a literal behind a field, method or subscript of a generic call's or
constructor's result: in `pick(Box { value: x }, wrap(1).value)` the `1` is a
`u64`, as it is with `Box { value: 1 }.value`, `wrap(1).get()` or `one(1)[0]`
there. It waits the same way behind a reference, a tuple, a call that hands its
argument back, or a constant tuple subscript. An operator on such a value is
checked against the type the literal settles to, so with `big: i128`,
`pick(Box { value: big }, -wrap(1).value)` negates an `i128`. A default that
reads the parameter is walked once it is settled, so `pick(Box { value: x }, f(1))` for
`fn f<T: Default>(a: T, b: T = T::default())` is `u64` throughout, and a field
default of a struct literal waits the same way. A `..base` settles its struct's
parameters as an annotation does. A struct literal waits only where the fields
it writes mention every parameter of its struct. Where they leave one out, the
literal settles where it stands, as it would outside a call.

A method's receiver waits as Rust's does. The method is found on the receiver's
head, and an impl's bounds are checked on the type the literal settles to,
wherever its target nests the parameter they bound. Where only one impl could
answer the method, it settles the literal: with `impl Tag for Box<u64>` alone,
`wrap(1).tag()` reads a `Box<u64>`. An impl could answer only where one type
for each literal makes its target the receiver, so `Pair<T, T>` answers
`mk(1, x)` at `x`'s type and `Pair<u64, i32>` never answers `dup(1)`. Inherent
impls are asked first, and the impls of the traits in scope only where no
inherent impl could answer. Where several could, the literal takes its default
first and the impl for that type answers. A subscript asks the impls that take
its key.

<!-- {"fixture":"spec_types_receiver_literal.wado"} -->

```wado
assert wrap(1).tag() == "u64";    // only `Box<u64>` could answer
assert wrap(1).name() == "i32";   // both could: the default answers
assert wrap(1 as u64).name() == "u64";
```

An impl is counted by its target's shape, not by whether a literal could become
its type. This is where Wado departs from Rust. Rust's integer literal becomes
one of a closed set of primitives, so it can rule out `Box<String>`. A Wado
literal also becomes a numeric newtype, a wide integer or a half-precision
float, and that set grows with the language. Counting by shape keeps method
lookup independent of the coercion rules. So with `impl Tag for Box<String>`
and `impl Tag for Box<u64>`, two impls could answer, the literal settles to
`i32`, and no impl takes `Box<i32>`. Writing the type the literal means
settles it before any impl is asked, as `wrap(1 as u64)` does above.

<!-- {"fixture":"spec_types_receiver_literal_counts_shape.wado"} -->

```wado
let t = wrap(1).tag();
```

A turbofish on the type name pins the arguments outright. It reaches a parameter
no field mentions, and it overrides one a field would otherwise settle. It says
what the matching annotation says, so the two must agree:

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
struct Tagged<T> { tag: i32 }
let b = Box::<i64> { value: 1 };        // T = i64, not the i32 the literal infers
let t = Tagged::<String> { tag: 7 };    // T names no field
let wide: i64 = b.value;
assert wide == 1 && t.tag == 7;
```

An annotation that disagrees with the turbofish is an error:

<!-- {"fixture":"spec_types_turbofish_disagrees.wado"} -->

```wado
let n: Box<i32> = Box::<i64> { value: 1 };
```

On a variant, the turbofish may follow the case name instead, as in Rust. It
means the same thing and names every parameter. Writing one on both the type
and the case is an error.

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
let a = Option::Some::<i64>(1);         // Option::<i64>::Some(1)
let e = Result::Err::<i32, String>("x");
let n: Option<i32> = None::<i32>;       // a bare case, where its type is known
assert a.unwrap() == 1 && e.is_err() && n == null;
```

A newtype over a variant names its base's cases, and the value takes the
newtype. A generic newtype's arguments are inferred as the variant's would be:

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
type Opt<T> = Option<T>;
let a = Opt::Some(1);                   // Opt<i32>
let b: Opt<i64> = Opt::None;            // the annotation settles T
let c = Opt::<i64>::Some(1);
assert a.unwrap() == 1 && b == null && c.unwrap() == 1;
```

### Scope of inference

| Site                   | Forward (from values) | Backward (from expected type) |
| ---------------------- | --------------------- | ----------------------------- |
| Struct literals        | yes                   | yes                           |
| Variant constructors   | yes                   | yes                           |
| Generic function calls | yes                   | yes                           |
| Generic method calls   | yes                   | yes                           |

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
fn identity<T>(x: T) -> T {
    return x;
}

fn none_of<T>() -> Option<T> {
    return null;
}

test "calls" {
    let x = identity(42);                   // T = i32 from the argument
    let s = identity("hi");                 // T = String from the argument
    let n: Option<i64> = none_of();         // T = i64 from the annotation
    assert x == 42 && s == "hi" && n == null;
}
```

A `_` inside a turbofish leaves that type-argument slot for inference while the
others stay explicit, reusing the same inference an omitted turbofish uses. The
explicit (non-`_`) arguments always win. An uninferable `_` is the same error as
an omitted turbofish on an uninferable parameter. A `_` works only in a
turbofish: in a plain type annotation (`let xs: List<_>`) it is an error.

A turbofish may also stop short of the declared parameters. The ones it does not
name are inferred, as a `_` in their place would be.

<!-- {"fixture":"spec_types_inference.wado"} -->

```wado
fn pick<A, B>(a: A, b: B) -> A {
    return a;
}

struct MyErr { code: i32 }

test "a partial turbofish" {
    let r = Result::<_, MyErr>::Ok(42);     // infers the Ok payload, pins the error type
    let a = pick::<_, bool>(1, true);       // infers the first type argument
    let b = pick::<i32>(1, true);           // stops short: infers the second
    assert r.unwrap() == 42 && a == 1 && b == 1;
}
```

## Enums, Variants, and Flags

Wado splits what Rust's `enum` covers into three kinds, as the Component Model
does. An `enum` has cases with no payload, a `variant` has cases that may carry
one, and a `flags` type is a set of bits. Each crosses a component boundary as
the Component Model type of the same name.

### Enums

<!-- {"fixture":"spec_types_enum.wado"} -->

```wado
enum Color {
    Red,
    Green,
    Blue,
}

test {
    let c = Color::Red;
    let d: Color = Red;   // bare where the expected type supplies it
    assert c == d;

    let name = match c {
        Red => "red",
        Green => "green",
        Blue => "blue",
    };
    assert name == "red";

    assert c matches { Red };
    assert !(c matches { Green | Blue });
}
```

A bare case with no expected type is an error:

<!-- {"fixture":"variant_error_bare_case_needs_context.wado"} -->

```wado
let red = Red;
```

An enum may have `impl` blocks:

<!-- {"fixture":"spec_types_enum.wado"} -->

```wado
impl Color {
    fn is_warm(&self) -> bool {
        return match *self {
            Red => true,
            _ => false,
        };
    }
}

test {
    assert Color::Red.is_warm() && !Color::Blue.is_warm();
}
```

### Variants

A variant case carries one payload type or none. Several values travel as one
tuple payload, `[T, U]`:

<!-- {"fixture":"spec_types_variant.wado"} -->

```wado
variant Shape {
    Circle(f64),           // one payload: the radius
    Rectangle([f64, f64]), // a tuple payload: width and height
    Point,                 // no payload
}

fn area(s: Shape) -> f64 {
    return match s {
        Circle(r) => 3.0 * r * r,
        Rectangle([w, h]) => w * h,
        Point => 0.0,
    };
}

test {
    let c: Shape = Circle(5.0);   // bare where the expected type supplies it
    assert area(c) == 75.0;
    assert area(Shape::Rectangle([10.0, 20.0])) == 200.0;
    assert area(Shape::Point) == 0.0;
}
```

A variant may be generic, and inside an `impl` `Self::Case` names a case of
the impl's own type:

<!-- {"fixture":"spec_types_variant.wado"} -->

```wado
variant Maybe<T> {
    Just(T),
    Nothing,
}

impl<T> Maybe<T> {
    // `Self::Case` names a case of the impl's own type. `Self` already carries
    // its type arguments, so the case takes no turbofish.
    fn wrap(v: T) -> Maybe<T> {
        return Self::Just(v);
    }
}

test {
    assert Maybe::wrap(1) matches { Just(1) };
}
```

In a case's payload type, `Self` names the variant being declared, as it names
the struct in a struct's field types. `Cons([i32, Self])` in `variant List`
says what `Cons([i32, List])` says.

A pattern destructures a tuple payload:

<!-- {"fixture":"spec_types_variant.wado"} -->

```wado
variant ParseResult {
    Fail,
    Number([i32, i32]),  // start and end positions
}

test {
    let result = ParseResult::Number([0, 10]);
    let Number([start, end]) = result else {
        panic("not a number");
    };
    assert end - start == 10;
    assert !(result matches { ParseResult::Fail });  // a case may be qualified
}
```

`Option<T>` and `Result<T, E>` are declared as variants in `core:prelude`.

### Flags

Each member of a flags type is one bit, in declaration order:

<!-- {"fixture":"spec_types_flags.wado"} -->

```wado
pub flags Perms {
    Read,     // bit 0: 1
    Write,    // bit 1: 2
    Execute,  // bit 2: 4
}

test {
    let rw = Perms::Read | Perms::Write;
    assert rw as u32 == 3;
    assert (rw & Perms::Read) as u32 == 1;   // masking
    assert (rw ^ Perms::Read) as u32 == 2;   // toggling
    assert Perms::none() as u32 == 0;
    assert Perms::all() as u32 == 7;
}
```

Arithmetic operators are not defined on flags; use the bitwise ones:

<!-- {"fixture":"flags_arith_add.wado"} -->

```wado
let r = Perms::Read;
let w = Perms::Write;
let bad = r + w;
```

A flags type is its own kind of type, not a newtype: it has none of `u32`'s
methods. Its bits are a `u32`, which `as` converts to and from, and an integer
literal coerces to it. A member may carry a `#[cm("...")]` attribute that names
it at a component boundary:

<!-- {"fixture":"spec_types_flags.wado"} -->

```wado
pub flags PathFlags {
    #[cm("symlink-follow")]
    SymlinkFollow,
}

test {
    assert PathFlags::SymlinkFollow as u32 == 1;
}
```

## Auto-derived Traits

A derived `Eq` or `Ord` compares by the type's shape:

- A struct compares field by field. `Ord` is lexicographic, in field
  declaration order.
- An enum compares its case. `Ord` follows declaration order.
- A flags type compares its raw bits.
- A variant compares its case first, then the payloads of one case. `Ord`
  puts an earlier case in declaration order before a later one, whatever their
  payloads.

When a derived impl exists, which instantiations of a generic type it covers,
and how a written impl overrides it are stated in
[Derivation Policy](./spec-traits.md#derivation-policy). Which types have
`Display`, a plain enum among them, is stated in
[Format Traits](./spec-traits.md#format-traits).

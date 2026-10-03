# Standard Traits

The prelude declares the traits below, and the language gives each a role:
`for-of` reads `IntoIterator`, `==` and `<` read `Eq` and `Ord`, `a + b` reads
`Add`, and `x[i]` reads the indexing traits. How a trait is declared,
implemented and bounded is in [Traits](./spec-traits.md).

## Iterator Traits

The prelude defines iterator traits for generic iteration over collections.

### Iterator - Core Iteration Trait

<!-- {"fixture":"spec_traits_iterator_decls.wado"} -->

```wado
/// Types that can yield a sequence of values
pub trait Iterator with () {
    type Item;

    /// Advances the iterator and returns the next value.
    /// Returns None when iteration is complete.
    fn next(&mut self) -> Option<Self::Item>;
}

test {
    let mut c = Countdown { n: 1 };
    assert c.next() == Option::Some(1) && c.next() == null;
}
```

### IntoIterator - Conversion Trait

<!-- {"fixture":"spec_traits_iterator_decls.wado"} -->

```wado
/// Types that can be converted into an iterator
pub trait IntoIterator with () {
    type Item;
    type Iter: Iterator<Item = Self::Item>;

    /// Creates an iterator from a value
    fn into_iter(&self) -> Self::Iter;
}

test {
    let mut it = Count { n: 2 }.into_iter();
    assert it.next() == Option::Some(2);
}
```

### FromIterator - Collection Construction

<!-- {"fixture":"spec_traits_iterator_decls.wado"} -->

```wado
/// Types that can be constructed from an iterator of `Elem`
pub trait FromIterator with () {
    type Elem;
    fn from_iter<I: Iterator<Item = Self::Elem>>(iter: &mut I) -> Self;
}

test {
    assert Total::from_iter(&mut Countdown { n: 3 }).sum == 6;
}
```

### Iterator Naming

Every standard library iterator that has a choice between yielding values and
yielding references names it, in the type and in the method:

| Axis    | Token    | Yields   | Method           |
| ------- | -------- | -------- | ---------------- |
| value   | `Value`  | `T`      | `iter_value()`   |
| shared  | `Ref`    | `&T`     | `iter_ref()`     |
| mutable | `RefMut` | `&mut T` | `iter_ref_mut()` |

No name leaves the axis unmarked, except `IntoIterator` / `into_iter`. They are
what `for-of` calls, and `for-of` marks the axis in its syntax (`of xs`,
`of &xs`, `of &mut xs`). A reference iterator's `iter_value()` turns it into the
value iterator over the same elements, where Rust says `copied()`.

The sequence family's iterators:

| Type                 | Item       | Reached by                                    |
| -------------------- | ---------- | --------------------------------------------- |
| `SliceValueIter<T>`  | `T`        | `iter_value()` on `Array`, `List`, or `Slice` |
| `SliceRefIter<T>`    | `&T`       | `iter_ref()`                                  |
| `SliceRefMutIter<T>` | `&mut T`   | `iter_ref_mut()` on `Array` or `List`         |
| `SliceWindows<T>`    | `Slice<T>` | `windows(size)`                               |
| `SliceChunks<T>`     | `Slice<T>` | `chunks(size)`                                |

The maps and sets of `core:collections` carry the same axis, and share one set
of iterators: a set's elements are the keys of a map to `()`. A map projection
needs no suffix, since `keys()` already names what it yields, and it yields
references:

| Type                        | Item       | Reached by                                       |
| --------------------------- | ---------- | ------------------------------------------------ |
| `MapKeysRefIter<K, V>`      | `&K`       | `keys()`, or a set's `iter_ref()`                |
| `MapKeysValueIter<K, V>`    | `K`        | `keys().iter_value()`, or a set's `iter_value()` |
| `MapValuesRefIter<K, V>`    | `&V`       | `values()`                                       |
| `MapValuesValueIter<K, V>`  | `V`        | `values().iter_value()`                          |
| `MapEntriesRefIter<K, V>`   | `[&K, &V]` | `entries()`                                      |
| `MapEntriesValueIter<K, V>` | `[K, V]`   | `entries().iter_value()`                         |

A map offers no `&mut` traversal: a `&mut` key would break the ordering, and a
`&mut` value buys nothing over `m[k] = v`. The map and set iterators refer to
the collection rather than copying it, so inserting or removing mid-traversal
can skip or repeat an entry. `iter_value().collect()` takes a snapshot.

### Terminals

Everything `Iterator` declares is available on every implementor, adapters included. That covers the terminals bounded by their element type: `sum` / `product` need `Item: Add<Output = Item>` / `Mul<Output = Item>`, and `min` / `max` need `Item: Ord`. See [`core:prelude`](./stdlib-core-prelude.md) for the full list and each one's behaviour.

### Usage

<!-- {"fixture":"spec_traits_iterator_usage.wado"} -->

```wado
let arr: List<i32> = [1, 2, 3, 4, 5];

// for-of uses IntoIterator automatically
for let x of arr {
    println(`${x}`);
}

// Explicit iterator
let mut iter = arr.iter_value();
while let Some(x) = iter.next() {
    println(`${x}`);
}

// Collect remaining elements
let mut rest_iter = arr.iter_value();
rest_iter.next();  // skip first
let rest = rest_iter.collect();
assert rest == [2, 3, 4, 5];

// Terminals compose with the adapters
let total = arr.iter_value().filter(|x| x % 2 == 1).sum();
assert total == Option::Some(9);
```

### Value Semantics

By-value iteration (`into_iter()`, `iter_value()`, `for let x of list`) returns copies of elements. Reference iteration yields references instead: `iter_ref()` and `for let x of &list` yield `&T`, and `iter_ref_mut()` and `for let x of &mut list` yield `&mut T`.

`&mut` iteration mutates elements in place. It needs an element type that is `RefMut` (see [Dispatch](#dispatch)), such as a `struct`, `List`, `String`, or `i128`/`u128`. A write through the `&mut T` lands on the element:

<!-- {"fixture":"spec_traits_iter_mut.wado"} -->

```wado
let mut points: List<Point> = [Point { x: 1, y: 0 }, Point { x: 2, y: 0 }];
for let p of &mut points {
    p.x += 1;  // mutates the element in place
}
assert points[0].x == 2 && points[1].x == 3;
```

An element type that is replaced on assignment (a primitive, `enum`, `flags`, `variant`, or `fn`) is not `RefMut`, so a write through `&mut T` would be lost. `&mut` iteration over such a list is a compile error; use indexed access instead:

<!-- {"fixture":"spec_traits_iter_mut.wado"} -->

```wado
let mut arr: List<i32> = [1, 2, 3];
for let mut i = 0; i < arr.len(); i += 1 {
    arr[i] = arr[i] * 2;
}
assert arr == [2, 4, 6];
```

Outside iteration too, `&mut` of such a field or element is an error, a
`variant` excepted
([Mutable References to Fields and Elements](./spec-memory.md#mutable-references-to-fields-and-elements)).

### Custom Iterables

Any type can be made iterable by implementing `IntoIterator`:

<!-- {"fixture":"spec_traits_custom_iterable.wado"} -->

```wado
struct Stack<T> { items: List<T> }
struct StackIter<T> { items: List<T>, index: i32 }

impl<T> Iterator for StackIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        if self.index == 0 { return null; }
        self.index -= 1;
        return Option::Some(self.items[self.index]);
    }
}

impl<T> IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;
    fn into_iter(&self) -> StackIter<T> { return StackIter { items: self.items, index: self.items.len() }; }
}

test {
    let stack = Stack { items: [1, 2, 3] };
    let mut seen: List<i32> = [];

    // Now for-of works
    for let x of stack { seen.push(x); }
    assert seen == [3, 2, 1];
}
```

### Iterator Combinators

Iterators support `map`, `filter`, and `fold` for functional-style data processing:

<!-- {"fixture":"spec_traits_combinators.wado"} -->

```wado
let arr: List<i32> = [1, 2, 3, 4, 5];

// map - transform each element
let doubled = arr.into_iter().map(|x| x * 2).collect();
assert doubled == [2, 4, 6, 8, 10];

// filter - keep elements matching predicate
let evens = arr.into_iter().filter(|x| x % 2 == 0).collect();
assert evens == [2, 4];

// fold - reduce to single value
let sum = arr.into_iter().fold(0, |acc, x| acc + x);
assert sum == 15;

// Chaining combinators
let result = arr.into_iter()
    .filter(|x| x > 2)
    .map(|x| x * 10)
    .collect();
assert result == [30, 40, 50];
```

## Sequence and AsSlice

Two prelude traits carry the shared methods of the
[sequence family](./spec-types.md#the-sequence-family), split by whether the
implementor has a contiguous backing. `Array`, `List` and `Slice` implement
both.

<!-- {"fixture":"spec_traits_sequence_decls.wado"} -->

```wado
pub trait Sequence with () {
    type Elem;

    fn len(&self) -> i32;
    fn get_unchecked(&self, index: i32) -> Self::Elem;

    // default bodies, written against `len` and `get_unchecked`
    fn is_empty(&self) -> bool { return self.len() == 0; }
    fn get(&self, index: i32) -> Option<Self::Elem> {
        return if index < 0 || index >= self.len() { null } else { Option::Some(self.get_unchecked(index)) };
    }
    fn first(&self) -> Option<Self::Elem> { return self.get(0); }
    fn last(&self) -> Option<Self::Elem> { return self.get(self.len() - 1); }
    fn position(&self, mut pred: fn mut(Self::Elem) -> bool) -> Option<i32> {
        for let mut i = 0; i < self.len(); i += 1 {
            if pred(self.get_unchecked(i)) { return Option::Some(i); }
        }
        return null;
    }
}

pub trait AsSlice: Sequence with () {
    fn as_slice(&self) -> Slice<Self::Elem>;

    // default bodies, through `as_slice`
    fn slice(&self, start: i32, end: i32) -> Slice<Self::Elem> { return self.as_slice().slice(start, end); }
    fn iter_value(&self) -> SliceValueIter<Self::Elem> { return self.as_slice().iter_value(); }
    fn iter_ref(&self) -> SliceRefIter<Self::Elem> { return self.as_slice().iter_ref(); }
    fn windows(&self, size: i32) -> SliceWindows<Self::Elem> { return self.as_slice().windows(size); }
    fn chunks(&self, size: i32) -> SliceChunks<Self::Elem> { return self.as_slice().chunks(size); }
}

test {
    let d = Digits { items: [0, 1, 2] };
    assert d.last() == Option::Some(2) && d.position(|x| x == 1) == Option::Some(1);
    assert d.slice(1, 3).len() == 2;
}
```

The element type is an associated type, so a bound reads
`S: Sequence<Elem = i32>`. A method that needs a bound on the element, such as
`contains` (`Elem: Eq`), is not a trait method: each type carries it as a
bounded inherent method. Mutation is in neither trait, since a `Slice` has no
mutable backing and length changes belong to `List` alone. `String` implements
neither; its bytes are viewed through `AsByteSlice`, and its text through
[`AsStrSlice`](#asstrslice).

A function that reads any of the three takes the trait by value:

<!-- {"fixture":"spec_traits_as_slice_bound.wado"} -->

```wado
fn total<S: AsSlice<Elem = i32>>(xs: S) -> i32 {
    return xs.iter_value().fold(0, |acc, x| acc + x);
}

test {
    let list: List<i32> = [1, 2, 3];
    assert total(list) == 6 && total(list.to_array()) == 6 && total(list.as_slice()) == 6;
}
```

Rationale: [WEP: The Sequence Family](./wep-2026-06-02-sequence-family.md).

## Builtin Comparison Traits

The prelude defines traits for comparison operators:

### Eq - Equality

<!-- {"fixture":"spec_traits_comparison_decls.wado"} -->

```wado
/// Types that can be compared for equality
pub trait Eq<Rhs = Self> with () {
    /// Returns true if self equals other
    fn eq(&self, other: &Rhs) -> bool;
}

test {
    assert Coin { cents: 5 }.eq(&Coin { cents: 5 });
}
```

The `==` and `!=` operators use `Eq::eq`:

- `a == b` desugars to `Eq::eq(&a, &b)`
- `a != b` desugars to `!Eq::eq(&a, &b)`

`Eq<Self>` is an equivalence: `a == a` holds, `a == b` is `b == a`, and `a == b`
with `b == c` gives `a == c`. A derived impl holds this when its members' impls
do. Nothing checks a written one.

`==` can span two types. The right operand picks among a type's `Eq<Rhs>` impls
exactly as it picks among its `Add<Rhs>` impls, so `StrSlice` and `String`
compare directly, in either order, with nothing copied.

A reference compared with a value asks the impls written for the reference
itself (`impl … for &T`), picked the same way. So a `&String` equals a `String`
through `impl<T: AsStrSlice> Eq<String> for &T`. Nothing dereferences the
reference: no impl answers `&i32 == i32`, so that comparison is an error.

Two references compare the values they point to, as in Rust. The prelude's
`impl<T: Eq> Eq for &T` (and the same for `&mut T`) answers `&a == &b` with
`a == b`, so `&T: Eq` holds wherever `T: Eq` does. `&mut` coerces to `&`, so a
`&mut` operand compares with a `&` one on either side. To ask whether two
references point to one place, call `ref_eq` (see
[Reference Identity](./spec-memory.md#reference-identity)).

### Ordering Enum

<!-- {"fixture":"spec_traits_ordering_decl.wado"} -->

```wado
/// Result of a three-way comparison
pub enum Ordering {
    Less,    // first value is less than second
    Equal,   // values are equal
    Greater, // first value is greater than second
}

test {
    assert Ordering::Less != Ordering::Greater;
}
```

### Ord - Ordering

<!-- {"fixture":"spec_traits_comparison_decls.wado"} -->

```wado
/// A total order over the type
pub trait Ord: Eq with () {
    /// Compares self with other and returns an Ordering
    fn cmp(&self, other: &Self) -> Ordering;
}

test {
    assert Coin { cents: 1 }.cmp(&Coin { cents: 2 }) == Ordering::Less;
}
```

`Ord` is a total order. `sort`, `TreeMap`, every `T: Ord` bound and the four
ordering operators read it, on every type:

- `a < b` desugars to `Ord::cmp(&a, &b) == Ordering::Less`
- `a > b` desugars to `Ord::cmp(&a, &b) == Ordering::Greater`
- `a <= b` desugars to `Ord::cmp(&a, &b) != Ordering::Greater`
- `a >= b` desugars to `Ord::cmp(&a, &b) != Ordering::Less`

So a comparison means the same in a body generic over `T: Ord` as at the
concrete type.

Where a type implements both `Eq` and `Ord`, `a == b` holds exactly when
`a.cmp(&b)` is `Ordering::Equal`. An impl the compiler writes holds this by
construction, since both come from one source
([Derivation Policy](./spec-traits.md#derivation-policy)).

A type may write both, so that `==` can answer faster than `cmp`, as a length
check does for a `String`. Nothing proves or checks that such a pair agrees.

Rationale: [WEP: One Order per Type](./wep-2026-09-23-comparison-traits.md).

### Float Comparison

`f16`, `bf16`, `f32` and `f64` share one equality and one order, which the
operators, `sort` and `TreeMap` all read:

- Every NaN is one value. It equals every NaN, whatever its sign and payload,
  and is greater than every other value, `+Inf` included.
- `-0.0` equals `0.0`.
- Any other two values compare as IEEE 754 compares them.

The answers differ from IEEE's only when an operand is a NaN. `NaN == NaN` is
true, `x < NaN` is true for any `x` that is not a NaN, and `NaN < x` stays
false. `x != x` is always false, so a NaN is tested with `is_nan()`.

Each of the four float types carries IEEE 754's six comparisons as methods.
They answer as IEEE does, so a NaN operand makes every one false except
`ieee754_ne`:

| Method            | IEEE predicate | Same as                  |
| ----------------- | -------------- | ------------------------ |
| `a.ieee754_eq(b)` | `a == b`       | `a == b && !a.is_nan()`  |
| `a.ieee754_ne(b)` | `a != b`       | `a != b \|\| a.is_nan()` |
| `a.ieee754_lt(b)` | `a < b`        | `a < b && !b.is_nan()`   |
| `a.ieee754_le(b)` | `a <= b`       | `a <= b && !b.is_nan()`  |
| `a.ieee754_gt(b)` | `a > b`        | `a > b && !a.is_nan()`   |
| `a.ieee754_ge(b)` | `a >= b`       | `a >= b && !a.is_nan()`  |

On `f16` and `bf16` they compare the operands widened to `f32`. No method
returns an IEEE three-way answer: IEEE's comparison is partial, and the order
above is the one three-way comparison a float has.

> Not yet implemented: the `ieee754_*` methods.

`min` and `max` follow the order wherever they are called: `f32::min`,
`f64::min`, `Iterator::min`, and every `min` over a `T: Ord`. NaN is greatest,
so `min(1.0, NaN)` is `1.0` and `max(1.0, NaN)` is NaN.

Of two `Equal` arguments, on every type, `min` returns the first and `max` the
second. `Iterator::min` returns the first of its least elements and
`Iterator::max` the last of its greatest, so `min(a, b)` is `[a, b]`'s `min()`.
`[min(a, b), max(a, b)]` is then `[a, b]` sorted stably: `min(-0.0, 0.0)` is
`-0.0` and `max(-0.0, 0.0)` is `0.0`.

`f32::ieee754_min`, `f32::ieee754_max`, `f64::ieee754_min` and
`f64::ieee754_max` are IEEE 754-2019 `minimum` and `maximum`: a NaN argument
gives NaN on both, and `-0.0` is less than `0.0`.

`clamp(x, low, high)` on `f32` and `f64` first traps on a NaN bound or on
`low > high`, whatever `x` is. It then confines `x` to `low..=high` by the
order, with one exception: a NaN `x` gives NaN rather than `high`.

### Default Implementations

`String` and `List<T>` implement `Eq` and `Ord` with lexicographic comparison:

<!-- {"fixture":"spec_traits_string_ord.wado"} -->

```wado
let a = "apple";
let b = "banana";
assert a < b;                    // lexicographic ordering
assert a == "apple" && a != b;   // byte-by-byte equality

let xs: List<i32> = [1, 2];
assert xs < [1, 3] && xs < [1, 2, 0];
```

## Default Trait

The prelude defines a `Default` trait providing a uniform "zero value" / "empty value" interface:

<!-- {"fixture":"spec_traits_default_decl.wado"} -->

```wado
pub trait Default with () {
    fn default() -> Self;
}

test {
    assert Origin::default().x == 0;
}
```

### Standard Library Implementations

| Type                                                                 | `default()` |
| -------------------------------------------------------------------- | ----------- |
| `i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`, `i128`, `u128` | `0`         |
| `f16`, `bf16`, `f32`, `f64`                                          | `0.0`       |
| `bool`                                                               | `false`     |
| `char`                                                               | `'\0'`      |
| `String`                                                             | `""`        |
| `Array<T>`, `List<T>`                                                | `[]`        |
| `Option<T>`                                                          | `null`      |
| `TreeMap<K, V>` (`K: Ord`)                                           | `{}`        |
| `TreeSet<T>` (`T: Ord`)                                              | `[]`        |

`Result<T, E>` does not implement `Default`, since there is no obvious choice between `Ok` and `Err`.

### Usage

<!-- {"fixture":"spec_traits_default_usage.wado"} -->

```wado
fn make_default<T: Default>() -> T { return T::default(); }

test {
    assert i32::default() == 0;
    assert String::default() == "";

    assert make_default::<i32>() == 0;
    assert make_default::<List<String>>() == [];
}
```

### Auto-Derivation

`Default` is derived for a non-generic struct whose every field declares a
default expression (`f: T = expr`; see
[Struct Field Defaults](./spec-types.md#struct-field-defaults)). A fieldless
struct qualifies, since it has exactly one value, so a marker like `NoFields`
can serve as a type parameter's default. A generic struct derives no `Default`:
a default expression is checked against the declaration, not against an
instantiation, so the struct needs a written impl.
[Derivation Policy](./spec-traits.md#derivation-policy) says where the impl is
derived, and that a written one wins.

<!-- {"fixture":"spec_traits_default_derive.wado"} -->

```wado
struct Config {
    host: String = "localhost",
    port: i32 = 8080,
}

test "derived" {
    let c = Config::default();
    assert c.host == "localhost" && c.port == 8080;
}
```

For other types, the user writes the impl manually:

<!-- {"fixture":"spec_traits_default_derive.wado"} -->

```wado
struct Point { x: i32, y: i32 }

impl Default for Point {
    fn default() -> Point { return Point { x: 0, y: 0 }; }
}

test "written" {
    assert Point::default().x == 0;
}
```

## String Parsing Traits

Two prelude traits parse a value from text, both taking any `AsStrSlice` and returning `Result`. `FromStr` is strict; `LenientFromStr` is forgiving of human input. `char`, `bool`, the integer types (`i128`/`u128` included), and the float types implement both; `String` implements only the lenient one, since taking a string as itself cannot fail.

<!-- {"fixture":"spec_traits_parse.wado"} -->

```wado
assert i32::from_str("42").unwrap() == 42;
assert i32::from_str("0x2A").is_err();                  // strict rejects the prefix

assert i32::from_str_lenient("0x2A").unwrap() == 42;    // radix prefixes 0x/0o/0b
assert i32::from_str_lenient("1_000").unwrap() == 1000; // `_` digit separators
assert bool::from_str_lenient("TRUE").unwrap();         // casing, plus 1/0
assert f64::from_str_lenient("inf").unwrap() == f64::INFINITY;
assert i32::from_str_lenient(" 1 ").is_err();           // never trims whitespace
```

A `StrSlice` is an `AsStrSlice`, so a field is parsed out of a larger buffer with no substring allocation (see [String Views](./spec-types.md#string-views)).

<!-- {"fixture":"spec_traits_parse_decls.wado"} -->

```wado
pub trait FromStr with () {
    type Err: Error;
    fn from_str<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
}

pub trait LenientFromStr with () {
    type Err: Error;
    fn from_str_lenient<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
}

test {
    assert Flag::from_str("on").unwrap().on && Flag::from_str("ON").is_err();
    assert Flag::from_str_lenient("ON").unwrap().on;
}
```

`Err: Error` on both, so a caller reaching a failure through the bound can
report its reason. The two are independent capabilities: a type implements
either, both, or neither, and neither implies the other. A newtype inherits both
from its base, so `type Port = u16` parses like `u16`.

### Leniency

`FromStr` is strict because machine-facing input depends on it: a JSON number
or a router path segment must reject `"TRUE"` and `"0x2A"`. `LenientFromStr`
widens the accepted spellings, never the accepted meanings: `"0x2A"` is `42`,
and `"forty-two"` is still `Err`. An impl never panics, and an input it cannot
read is `Err`.

It never trims whitespace: trimming is the caller's choice. A type whose
spellings admit no whitespace rejects it, so `" 1 "` is `Err` for an integer,
and a value whose whitespace is significant, a `char` `' '` or an indented
`String`, survives.

The built-in impls accept:

| Type                        | Accepted                                                                                 |
| --------------------------- | ---------------------------------------------------------------------------------------- |
| `String`                    | any string, as itself                                                                    |
| `char`                      | exactly one Unicode scalar, as `FromStr` does                                            |
| `i8` … `i128`               | decimal, or `0x` / `0o` / `0b` in either case; an optional leading `+` or `-`            |
| `u8` … `u128`               | as the signed types, but a leading `-` is `Err`                                          |
| `f16`, `bf16`, `f32`, `f64` | decimal with an optional exponent; `nan`, `inf`, `infinity`, with a sign and in any case |
| `bool`                      | `true` / `false` in any case, `1` / `0`                                                  |

An integer or float ignores `_` anywhere in its digits (`1_000`, `0xFF_FF`), as
a Wado numeric literal does. `,` is not a separator. A leading zero does not
mean octal: `010` is `10`, and only `0o12` is octal. Every built-in impl's `Err`
is `LenientParseError`.

Rationale: [WEP: Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md).

## AsStrSlice

`AsStrSlice` is the conversion that lets one signature take an owned `String`, a
reference to one, or a [`StrSlice`](./spec-types.md#string-views) view of one:

<!-- {"fixture":"spec_traits_as_str_slice_decl.wado"} -->

```wado
pub trait AsStrSlice: Eq<String> with () {
    fn as_str_slice(&self) -> StrSlice;
    // default bodies over the view: len, slice, chars, starts_with, find, …
}

test {
    assert Word { text: "hi" }.as_str_slice() == "hi";
}
```

`String` and `StrSlice` implement it, and `impl<T: AsStrSlice> AsStrSlice for &T`
passes a reference through. A parameter that only reads its text names
`AsStrSlice` and takes it by value, so a call site passes a literal bare:
`f("banana")`.

`AsStrSlice` requires `Eq<String>`, so a body generic over it compares its text
with `==` against a string, and a string-literal pattern matches it. `==` on a
type parameter reads the parameter's bounds, so without the requirement neither
would resolve.

Rationale: [WEP: String Views](./wep-2026-09-13-string-slice.md).

## Arithmetic Operator Traits

The prelude's binary operator traits (`Add`, `Sub`, `Mul`, `Div`, `Rem`,
`BitAnd`, `BitOr`, `BitXor`) carry a right-hand type parameter defaulting to
`Self`:

<!-- {"fixture":"spec_traits_add_decl.wado"} -->

```wado
trait Add<Rhs = Self> with () {
    type Output;
    fn add(&self, rhs: &Rhs) -> Self::Output;
}

test {
    assert Cm { v: 1 }.add(&Cm { v: 2 }).v == 3;
}
```

Omitting the argument is the ordinary case: `impl Add for Meters` adds two
`Meters`. Writing it lets one type be added to another, and the right operand
selects between the impls:

<!-- {"fixture":"spec_traits_add_rhs.wado"} -->

```wado
impl Add for Meters {              // Meters + Meters
    type Output = Meters;
    fn add(&self, rhs: &Meters) -> Meters { return Meters { v: self.v + rhs.v }; }
}
impl Add<Feet> for Meters {        // Meters + Feet
    type Output = Meters;
    fn add(&self, rhs: &Feet) -> Meters { return Meters { v: self.v + rhs.v * 0.3048 }; }
}

test {
    let m = Meters { v: 1.0 };
    let f = Feet { v: 10.0 };
    let total = m + f;                 // selects Add<Feet>
    assert total.v == 4.048 && (m + m).v == 2.0;
}
```

Selection follows the same unique-or-error rule as a method call's argument
lists (see [One Trait at Two Argument Lists](./spec-traits.md#one-trait-at-two-argument-lists)).
`Neg` and `BitNot` are unary and take no argument; `Shl` / `Shr` declare
`rhs: u32`.

The compiler supplies these impls for the integers, and for `f32` / `f64`
except `Rem`. An unsigned integer has no `Neg`, as in Rust: `x.wrapping_neg()`
negates it modulo its width. `bool` holds one bit, so it gets the bit operators and no shift;
`v128` gets none, since its arithmetic is lane-wise and only a lane type's own
impl knows it.

An operator yields `Output`, which a widening impl may make another type, so a
generic body folding back into its own parameter pins it:

<!-- {"fixture":"spec_traits_output_pin.wado"} -->

```wado
fn sum2<T: Add<Output = T>>(a: T, b: T) -> T { return a + b; }
fn scale<T: Mul>(a: T, b: T) -> T::Output { return a * b; }

test {
    assert sum2(2, 3) == 5 && scale(2.0, 4.0) == 8.0;
}
```

`T::Output` under two bounds that both declare `Output` is ambiguous unless
they bind it to the same type.

An operator names these traits by construction, not by spelling: a trait
declared as `Add` elsewhere shadows the name but does not answer `+`.

## Indexing Traits

The prelude defines traits for index-based access:

### IndexValue - Value Read

<!-- {"fixture":"spec_traits_index_decls.wado"} -->

```wado
/// Returns element by value (copy)
pub trait IndexValue<IndexType> with () {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}

test {
    assert Cells { v: [Cell { n: 7 }] }.index_value(0).n == 7;
}
```

### IndexAssign - Value Write

<!-- {"fixture":"spec_traits_index_decls.wado"} -->

```wado
/// Assigns value to element at index
pub trait IndexAssign<IndexType> with () {
    type Output;
    fn index_assign(&mut self, index: IndexType, value: Self::Output);
}

test {
    let mut c = Cells { v: [Cell { n: 7 }] };
    c.index_assign(0, Cell { n: 8 });
    assert c.v[0].n == 8;
}
```

### IndexRef - Reference Read

<!-- {"fixture":"spec_traits_index_decls.wado"} -->

```wado
/// Returns element by shared reference
pub trait IndexRef<IndexType> with () {
    type Output: Ref;
    fn index_ref(&self, index: IndexType) -> &Self::Output;
}

test {
    assert Cells { v: [Cell { n: 7 }] }.index_ref(0).n == 7;
}
```

### IndexRefMut - Mutable Reference

<!-- {"fixture":"spec_traits_index_decls.wado"} -->

```wado
/// Returns element by mutable reference
pub trait IndexRefMut<IndexType> with () {
    type Output: RefMut;
    fn index_ref_mut(&mut self, index: IndexType) -> &mut Self::Output;
}

test {
    let mut c = Cells { v: [Cell { n: 7 }] };
    c.index_ref_mut(0).n = 9;
    assert c.v[0].n == 9;
}
```

### Dispatch

A use site reads the trait that matches what it does with the element:

- A bare read `c[i]` copies the element out through `IndexValue`.
- An assignment `c[i] = v` writes through `IndexAssign`.
- `&c[i]` and a `&self` receiver take `IndexRef` when the container has it, and a copy otherwise.
- A `&mut self` receiver, a field write `c[i].f = v`, and a compound one `c[i].f += v` take `IndexRefMut`. On a container without it they are compile errors.

A write assigns into the element rather than over it, so every subscript the
target passes through is a `&mut` place: `o[i][j] = v` and `o[i][j].f = v` take
`IndexRefMut` on `o[i]`. Neither other trait serves there. `IndexRef` hands out
a shared reference, which a write cannot go through, and `IndexValue` hands out
a copy, on which the write would be lost.

The four traits are independent: a container implements only the ones it
supports. A subscript names the prelude's declarations, as an operator does, so
a user trait spelled `IndexValue` does not answer `c[i]`.

`Ref` and `RefMut` are sealed marker traits the compiler provides for every
eligible type, and a user `impl Ref` or `impl RefMut` is a compile error:

| Types                                                                         | `Ref` | `RefMut` |
| ----------------------------------------------------------------------------- | ----- | -------- |
| `struct`, `List<T>`, `String`, tuples, `TreeMap` / `TreeSet`, `i128` / `u128` | yes   | yes      |
| `variant`, `fn`                                                               | yes   | no       |
| `&T`, `&mut T`                                                                | yes   | yes      |
| scalars, `enum`, `flags`                                                      | no    | no       |
| `resource`                                                                    | no    | no       |
| `()`, `!`                                                                     | no    | no       |

`Ref` holds for a type whose value `&T` can alias. `RefMut` holds for the `Ref`
types mutated in place rather than replaced on assignment, which excludes
`variant` and `fn`. A `resource` is a handle that cannot be aliased, so a
resource element is read by value. A newtype follows its base. Neither marker
asks whether a value holds references: a `struct` with `&T` fields is `Ref`, and
`i32` is not `Ref`, although `&i32` is.

The markers bound the traits' `Output`, and an impl must meet the bound:
`impl IndexRef<i32> for C { type Output = i32; … }` is a compile error, since a
scalar cannot back the reference it promises. They do not restrict the `&`
operator. `&nums[i]` on a `List<i32>` stays legal, and under value semantics it
is a reference to a copy.

The standard containers implement:

| Container       | `IndexValue` | `IndexAssign` | `IndexRef` | `IndexRefMut` |
| --------------- | ------------ | ------------- | ---------- | ------------- |
| `List<T>`       | every `T`    | every `T`     | `T: Ref`   | `T: RefMut`   |
| `Array<T>`      | every `T`    | every `T`     | `T: Ref`   | `T: RefMut`   |
| `Slice<T>`      | every `T`    | —             | `T: Ref`   | —             |
| `TreeMap<K, V>` | every `V`    | every `V`     | `V: Ref`   | `V: RefMut`   |

`List`, `Array` and `Slice` are indexed by `i32` and by a range, which yields a
`Slice<T>` (see [The Sequence Family](./spec-types.md#the-sequence-family)); `TreeMap` by `K`.
A slice is a shared view, so it has nothing to write through.

<!-- {"fixture":"spec_traits_index_dispatch.wado"} -->

```wado
let mut arr: List<i32> = [1, 2, 3];
let x = arr[0];    // IndexValue::index_value
arr[1] = 100;      // IndexAssign::index_assign
assert x == 1 && arr == [1, 100, 3];
```

Rationale: [WEP: Indexing Traits Design](./wep-2026-01-20-indexing-traits.md).

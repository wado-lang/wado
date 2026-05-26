# WEP: Iterator Traits Design

## Context

Wado needs iterator traits to enable:

1. **Generic iteration**: `for-of` loops over any iterable type
2. **Iterator combinators**: `map`, `filter`, `fold`, `collect`, etc.
3. **Literal coercion**: `[1, 2, 3]` → `Array<i32>` via `FromIterator`
4. **User-defined iterables**: Custom collections can be iterated

### Current State

- **For-of loops**: Hardcoded to only work with `Array<T>`
- **Tuple-to-Array coercion**: Hardcoded special-case logic
- **Associated types**: Implemented and working
- **Trait bounds**: Not yet implemented

### Differences from Rust

Wado's GC-based memory model significantly simplifies iterator design:

| Aspect            | Rust                                  | Wado                            |
| ----------------- | ------------------------------------- | ------------------------------- |
| Memory            | Ownership + borrowing                 | GC-managed                      |
| Lifetimes         | Required on iterators                 | Not needed                      |
| Iterator variants | `iter()`, `iter_mut()`, `into_iter()` | `iter()` only                   |
| Item ownership    | Borrowed or owned                     | Always copied (value semantics) |
| Trait bounds      | Required for `Iter: Iterator`         | Not yet available               |

### Comparison with JavaScript

Wado's iterator model is closer to JavaScript than Rust. Both use a two-layer abstraction:

| JavaScript                           | Wado                             | Role                       |
| ------------------------------------ | -------------------------------- | -------------------------- |
| **Iterable** (`[Symbol.iterator]()`) | **IntoIterator** (`into_iter()`) | Can produce an iterator    |
| **Iterator** (`next()`)              | **Iterator** (`next()`)          | Yields elements one by one |

```javascript
// JavaScript
const arr = [1, 2, 3];
const iter = arr[Symbol.iterator]();  // Iterable → Iterator
iter.next();  // { value: 1, done: false }
iter.next();  // { value: 2, done: false }
```

```wado
// Wado
let arr: Array<i32> = [1, 2, 3];
let mut iter = arr.into_iter();  // IntoIterator → Iterator
iter.next();  // Option::Some(1)
iter.next();  // Option::Some(2)
```

Both languages desugar `for-of` loops the same way: call the conversion method to get an iterator, then repeatedly call `next()` until exhausted.

Rust requires three iteration methods (`iter()`, `iter_mut()`, `into_iter()`) because of ownership semantics. Wado, like JavaScript, uses GC-managed memory with value semantics, so a single `iter()` / `into_iter()` suffices.

### Iterator vs IntoIterator: Role Distinction

Understanding the difference between `Iterator` and `IntoIterator`:

| Trait            | Question it answers                    | Example types                                |
| ---------------- | -------------------------------------- | -------------------------------------------- |
| **Iterator**     | "Can I call `next()` on this?"         | `ArrayIter<T>`, `RangeExclusive<T>`, `Chars` |
| **IntoIterator** | "Can I convert this into an iterator?" | `Array<T>`, `String`, `Stack<T>`             |

A collection (like `Array<T>`) is **not** an iterator itself—it doesn't have iteration state. Instead, it implements `IntoIterator` to create a separate iterator object that tracks the current position:

```wado
// Array<T> implements IntoIterator, NOT Iterator
let arr: Array<i32> = [1, 2, 3];

// arr.next() would NOT work - Array has no next() method
// Instead, convert to an iterator first:
let mut iter: ArrayIter<i32> = arr.into_iter();

// ArrayIter<T> implements Iterator
iter.next();  // Some(1) - advances internal index from 0 to 1
iter.next();  // Some(2) - advances internal index from 1 to 2
iter.next();  // Some(3) - advances internal index from 2 to 3
iter.next();  // None - exhausted
```

Some types (like `RangeExclusive`) are both a collection and their own iterator:

```wado
// RangeExclusive implements BOTH IntoIterator and Iterator
let r = 1..<4;
// r.into_iter() returns self (RangeExclusive is its own iterator)
// r.next() works directly
```

## Decision

### 1. Core Iterator Trait

```wado
/// The core iterator trait for sequences of values
pub trait Iterator {
    type Item;

    /// Advances the iterator and returns the next value.
    /// Returns None when iteration is complete.
    fn next(&mut self) -> Option<Self::Item>;
}
```

Key differences from Rust:

- No `size_hint()` initially (can add later for optimization)
- No lifetime parameters needed
- `Self::Item` is always owned/copied (value semantics)

### 2. IntoIterator Trait

```wado
/// Conversion into an Iterator
pub trait IntoIterator {
    type Item;
    type Iter;  // Note: No trait bound until bounds are implemented

    /// Creates an iterator from a value
    fn into_iter(self) -> Self::Iter;
}
```

**Design Note**: In Rust, `type Iter: Iterator<Item = Self::Item>` has a trait bound. Since Wado doesn't have trait bounds yet, we omit it. The compiler will check this constraint at call sites instead.

### 3. FromIterator Trait

```wado
/// Create a collection from an iterator
pub trait FromIterator<T> {
    /// Creates a value from an iterator
    fn from_iter(iter: impl Iterator) -> Self;
}
```

**Simplification**: Instead of `fn from_iter<I: Iterator<Item = T>>(iter: I)`, we use `impl Iterator`. The compiler checks element type compatibility at usage sites.

### 4. No `iter_mut()` - Wasm GC Limitation

In Rust, there are three ways to iterate:

- `iter()` - borrows elements (`&T`)
- `iter_mut()` - mutably borrows elements (`&mut T`)
- `into_iter()` - takes ownership (`T`)

In Wado:

- **`iter()`** - Returns iterator yielding element copies
- **No `iter_mut()`** - Impossible due to Wasm GC limitations
- **`into_iter()`** - Same as `iter()` for most types (no ownership transfer)

#### Why `iter_mut()` is Impossible

Wasm GC's `array.get` and `array.set` instructions only support **value copy** operations. You cannot obtain a reference to an array element:

```wado
let mut arr: Array<i32> = [1, 2, 3];

// This is impossible in Wasm GC:
let r: &mut i32 = &mut arr[0];  // ❌ Cannot get reference to array element
```

This is why Wado has separate traits for indexing:

- `IndexValue<I>` - Returns element by value (copy)
- `Index<I>` - Returns element by reference (only for reference-type elements)

For `Array<i32>`, only `IndexValue` can be implemented, not `Index`. Since `iter_mut()` would need to yield `&mut T`, it's fundamentally impossible for primitive arrays.

#### Mutation via Indexed Access

For in-place mutation, use indexed access:

```wado
// Wado: iter() returns copies (value semantics)
let arr: Array<i32> = [1, 2, 3];
for let x of arr.iter() {
    // x is a copy of each element
}

// For mutation, use indexed access
for let mut i = 0; i < arr.len(); i += 1 {
    arr[i] = arr[i] * 2;
}
```

This pattern works universally for all element types and is idiomatic in Wado.

### 5. Array Iterator Implementation

```wado
/// Iterator over Array<T> elements
pub struct ArrayIter<T> {
    array: Array<T>,
    index: i32,
}

impl ArrayIter<T> {
    pub fn new(array: Array<T>) -> ArrayIter<T> {
        return ArrayIter { array, index: 0 };
    }
}

impl Iterator for ArrayIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.array.len() {
            return null;
        }
        let item = self.array.get(self.index);
        self.index += 1;
        return Option::<T>::Some(item);
    }
}

impl IntoIterator for Array<T> {
    type Item = T;
    type Iter = ArrayIter<T>;

    fn into_iter(self) -> ArrayIter<T> {
        return ArrayIter::new(self);
    }
}

impl Array<T> {
    /// Returns an iterator over the array elements
    pub fn iter(&self) -> ArrayIter<T> {
        // Note: self is copied (value semantics), so iterator owns a copy
        return ArrayIter::new(*self);
    }
}
```

**Note**: Due to value semantics, `iter()` creates a copy of the array. This is fine for small arrays but may be inefficient for large ones. Future optimization: share the underlying `repr` field.

### 6. For-Of Loop Desugaring

Currently for-of is hardcoded for `Array<T>`. It should be generalized:

```wado
// Source
for let item of collection {
    body(item);
}

// Desugars to
scope: {
    let mut __iter = collection.into_iter();
    loop {
        if let Some(__item) = __iter.next() {
            let item = __item;
            body(item);
        } else {
            break;
        }
    }
}
```

**Fallback Strategy** (until trait bounds work):

1. Check if type has `into_iter()` method
2. Check if result has `next()` method returning `Option<T>`
3. Infer `Item` type from `Option<T>`

### 7. Iterator Combinator Methods

Iterator combinators are defined as methods on `Iterator`. Initially, implement as standalone functions, then migrate to default trait methods when supported.

#### Phase 1: Essential Combinators (Standalone)

```wado
/// Transforms each element using a function
pub fn map<T, U>(iter: impl Iterator, f: Fn(T) -> U) -> MapIter<T, U> {
    return MapIter { iter, f };
}

/// Filters elements based on a predicate
pub fn filter<T>(iter: impl Iterator, pred: Fn(&T) -> bool) -> FilterIter<T> {
    return FilterIter { iter, pred };
}

/// Reduces the iterator to a single value
pub fn fold<T, Acc>(iter: impl Iterator, init: Acc, f: Fn(Acc, T) -> Acc) -> Acc {
    let mut acc = init;
    loop {
        if let Some(item) = iter.next() {
            acc = f(acc, item);
        } else {
            break;
        }
    }
    return acc;
}

/// Collects iterator elements into a collection
pub fn collect<T, C: FromIterator<T>>(iter: impl Iterator) -> C {
    return C::from_iter(iter);
}
```

#### Phase 2: Methods on Iterator Trait (Future)

When default trait methods are implemented:

```wado
trait Iterator {
    type Item;

    fn next(&mut self) -> Option<Self::Item>;

    // Default methods
    fn map<U>(self, f: Fn(Self::Item) -> U) -> MapIter<Self, Fn> {
        return MapIter { iter: self, f };
    }

    fn filter(self, pred: Fn(&Self::Item) -> bool) -> FilterIter<Self, Fn> {
        return FilterIter { iter: self, pred };
    }

    fn fold<Acc>(self, init: Acc, f: Fn(Acc, Self::Item) -> Acc) -> Acc {
        // implementation
    }

    fn collect<C: FromIterator<Self::Item>>(self) -> C {
        return C::from_iter(self);
    }

    fn count(self) -> i32 {
        return self.fold(0, |acc, _| acc + 1);
    }

    fn sum(self) -> Self::Item {
        // Requires Add trait bound
    }
}
```

### 8. Combinator Iterator Types

```wado
/// Map iterator - transforms elements
pub struct MapIter<I, F> {
    iter: I,
    f: F,
}

impl Iterator for MapIter<I, F> {
    type Item = U;  // Output type of F

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.iter.next() {
            return Option::Some((self.f)(item));
        }
        return null;
    }
}

/// Filter iterator - yields only matching elements
pub struct FilterIter<I, P> {
    iter: I,
    pred: P,
}

impl Iterator for FilterIter<I, P> {
    type Item = T;  // Same as I::Item

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.iter.next() {
                if (self.pred)(&item) {
                    return Option::Some(item);
                }
            } else {
                return null;
            }
        }
    }
}
```

### 9. Tuple IntoIterator (Homogeneous Only)

Homogeneous tuples implement `IntoIterator`:

```wado
/// Iterator over tuple elements
pub struct TupleIter<T> {
    elements: Array<T>,
    index: i32,
}

impl Iterator for TupleIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.index >= self.elements.len() {
            return null;
        }
        let item = self.elements.get(self.index);
        self.index += 1;
        return Option::<T>::Some(item);
    }
}

// Compiler generates these for homogeneous tuples:
impl IntoIterator for [T, T] {
    type Item = T;
    type Iter = TupleIter<T>;

    fn into_iter(self) -> TupleIter<T> {
        let elements: Array<T> = [self.0, self.1];
        return TupleIter { elements, index: 0 };
    }
}

impl IntoIterator for [T, T, T] {
    type Item = T;
    type Iter = TupleIter<T>;

    fn into_iter(self) -> TupleIter<T> {
        let elements: Array<T> = [self.0, self.1, self.2];
        return TupleIter { elements, index: 0 };
    }
}

// ... up to reasonable tuple size (e.g., 12)
```

### 10. Range Iterator

See [WEP: Range Object](./wep-2026-03-03-range-object.md) for the full design.

```wado
/// Half-open range [start, end)
pub struct RangeExclusive<T> {
    pub start: T,
    pub end: T,
}

impl Iterator for RangeExclusive<T: Step + Ord> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.start >= self.end {
            return null;
        }
        let current = self.start;
        if let Some(next) = current.next_step() {
            self.start = next;
        } else {
            self.start = self.end;
        }
        return Option::<T>::Some(current);
    }
}

impl IntoIterator for RangeExclusive<T: Step + Ord> {
    type Item = T;
    type Iter = RangeExclusive<T>;

    fn into_iter(&self) -> RangeExclusive<T> {
        return *self;
    }
}
```

Usage:

```wado
// Iterate from 0 to 9
for let i of 0..<10 {
    println(`{i}`);
}

// With combinators
let sum = (1..<101).fold(0, |acc: i32, x: i32| acc + x);  // 5050
```

### 11. String Iterator

```wado
/// Iterator over string characters (Unicode code points)
pub struct Chars {
    string: String,
    byte_index: i32,
}

impl String {
    pub fn chars(&self) -> Chars {
        return Chars { string: *self, byte_index: 0 };
    }
}

impl Iterator for Chars {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if self.byte_index >= self.string.len() {
            return null;
        }
        // Decode UTF-8 and advance byte_index
        let (codepoint, bytes_consumed) = decode_utf8(self.string, self.byte_index);
        self.byte_index += bytes_consumed;
        return Option::<char>::Some(codepoint);
    }
}
```

### 12. Empty and Once Iterators

```wado
/// An iterator that yields nothing
pub struct Empty<T> {}

impl Iterator for Empty<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        return null;
    }
}

pub fn empty<T>() -> Empty<T> {
    return Empty {};
}

/// An iterator that yields exactly one element
pub struct Once<T> {
    item: Option<T>,
}

impl Iterator for Once<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if let Some(item) = self.item {
            self.item = null;
            return Option::<T>::Some(item);
        }
        return null;
    }
}

pub fn once<T>(item: T) -> Once<T> {
    return Once { item: Option::<T>::Some(item) };
}
```

### 13. Enumerate Iterator

```wado
/// Iterator that yields (index, element) pairs
pub struct Enumerate<I> {
    iter: I,
    index: i32,
}

impl Iterator for Enumerate<I> {
    type Item = [i32, T];  // Tuple of index and element

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(item) = self.iter.next() {
            let idx = self.index;
            self.index += 1;
            return Option::Some([idx, item]);
        }
        return null;
    }
}

// Usage
for let [i, x] of arr.iter().enumerate() {
    println(`{i}: {x}`);
}
```

### 14. Implementation Phases

#### Phase 1: Minimal Core (No Compiler Changes)

Define traits and Array implementation in prelude:

- `Iterator` trait with `next()`
- `IntoIterator` trait
- `ArrayIter<T>` struct
- `impl Iterator for ArrayIter<T>`
- `impl IntoIterator for Array<T>`
- `Array::iter()` method

For-of loop remains hardcoded for `Array<T>`.

#### Phase 2: For-Of Generalization

Modify compiler to:

1. Resolve `for-of` via `IntoIterator` trait lookup
2. Desugar to `into_iter()` + `next()` loop
3. Remove hardcoded `Array<T>` check

#### Phase 3: Tuple IntoIterator

Compiler-generated `IntoIterator` impls for homogeneous tuples.

#### Phase 4: FromIterator and Coercion

- `FromIterator<T>` trait
- `impl FromIterator<T> for Array<T>`
- Replace hardcoded tuple-to-array coercion with trait-based coercion

#### Phase 5: Iterator Combinators

Add combinator types and methods:

- `MapIter`, `FilterIter`, `Enumerate`, etc.
- Either as standalone functions or trait default methods

#### Phase 6: collect() and Full Chain

- `collect()` method on Iterator
- End-to-end: `[1,2,3].iter().filter(...).map(...).collect()`

### 15. Known Compiler Limitations

#### Parser: `self` by Value Not Supported

The parser does not support `self` (by value) in trait method parameters, only `&self` and `&mut self`. This affects the `IntoIterator` trait:

```wado
// Ideally:
fn into_iter(self) -> Self::Iter;

// Workaround (current):
fn into_iter(&self) -> Self::Iter;
```

This is a parser limitation, not a fundamental design issue.

#### Elaborator: Generic Associated Types in Return Position

When a generic struct `Foo<T>` implements a trait with `type Item = T`, and a trait method returns `Option<Self::Item>`, the type resolution fails. This affects all iterator implementations:

```wado
// This pattern fails to resolve correctly:
impl Iterator for ArrayIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> { ... }  // Error: Unknown type
}
```

**Status**: Compiler bug. Iterator tests are marked as TODO until fixed.

**Workaround**: None currently. Iterators must wait for this bug to be fixed.

## Consequences

### Positive

1. **Simplicity**: No lifetimes or ownership complexity
2. **Familiar**: Rust-like API for iterator chains
3. **Extensible**: User types can implement `Iterator`/`IntoIterator`
4. **Unified for-of**: Works for any `IntoIterator` type
5. **No special cases**: Array coercion via standard traits

### Negative

1. **Value copy overhead**: Each `next()` copies the element
   - **Mitigation**: Compiler can optimize for primitives; large structs should use references
2. **No `iter_mut()`**: Wasm GC cannot yield `&mut T` for array elements
   - **Mitigation**: Use indexed access for in-place modification
3. **Trait bounds missing**: `type Iter: Iterator` constraint not enforced
   - **Mitigation**: Check at usage sites until bounds are implemented

### Trade-offs

| Aspect                    | Rust                                  | Wado                               |
| ------------------------- | ------------------------------------- | ---------------------------------- |
| Iteration flexibility     | `iter()`, `iter_mut()`, `into_iter()` | `iter()` only                      |
| Zero-cost iteration       | Yes (references)                      | No (copies, but GC handles memory) |
| Lifetime complexity       | High                                  | None                               |
| In-place mutation         | `iter_mut()`                          | Indexed access                     |
| Implementation difficulty | High                                  | Low                                |

## Examples

### Basic Iteration

```wado
use {println, Stdout} from "core:cli";

fn run() with Stdout {
    let arr: Array<i32> = [1, 2, 3, 4, 5];

    // Using for-of (desugars to IntoIterator)
    for let x of arr {
        println(`{x}`);
    }

    // Explicit iterator
    let mut iter = arr.iter();
    while let Some(x) = iter.next() {
        println(`{x}`);
    }
}
```

### Iterator Combinators

```wado
fn run() with Stdout {
    let numbers: Array<i32> = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

    // Filter and map
    let even_squares: Array<i32> = numbers
        .iter()
        .filter(|x| x % 2 == 0)
        .map(|x| x * x)
        .collect();

    // [4, 16, 36, 64, 100]
    for let x of even_squares {
        println(`{x}`);
    }

    // Sum using fold
    let sum = numbers.iter().fold(0, |acc, x| acc + x);
    println(`Sum: {sum}`);  // Sum: 55
}
```

### Range Iteration

```wado
fn run() with Stdout {
    // Sum of 1 to 100
    let sum = (1..<101).fold(0, |acc: i32, x: i32| acc + x);
    println(`Sum 1-100: {sum}`);  // 5050

    // Generate squares
    let squares: Array<i32> = (1..<6)
        .map(|x: i32| x * x)
        .collect();
    // [1, 4, 9, 16, 25]
}
```

### Custom Iterator

```wado
struct Fibonacci {
    a: i64,
    b: i64,
}

impl Fibonacci {
    fn new() -> Fibonacci {
        return Fibonacci { a: 0, b: 1 };
    }
}

impl Iterator for Fibonacci {
    type Item = i64;

    fn next(&mut self) -> Option<i64> {
        let current = self.a;
        let next = self.a + self.b;
        self.a = self.b;
        self.b = next;
        return Option::<i64>::Some(current);
    }
}

fn run() with Stdout {
    let fib = Fibonacci::new();
    // Take first 10 Fibonacci numbers
    let fibs: Array<i64> = fib.take(10).collect();
    for let f of fibs {
        println(`{f}`);
    }
}
```

### User-Defined Collection

```wado
struct Stack<T> {
    items: Array<T>,
}

impl Stack<T> {
    fn new() -> Stack<T> {
        return Stack { items: [] };
    }

    fn push(&mut self, item: T) {
        self.items.push(item);
    }
}

// Make Stack iterable
struct StackIter<T> {
    items: Array<T>,
    index: i32,
}

impl Iterator for StackIter<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.index < 0 {
            return null;
        }
        let item = self.items.get(self.index);
        self.index -= 1;
        return Option::<T>::Some(item);
    }
}

impl IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;

    fn into_iter(self) -> StackIter<T> {
        // Iterate in LIFO order (top to bottom)
        return StackIter {
            items: self.items,
            index: self.items.len() - 1,
        };
    }
}

// Now for-of works with Stack
fn run() with Stdout {
    let mut stack: Stack<i32> = Stack::new();
    stack.push(1);
    stack.push(2);
    stack.push(3);

    for let x of stack {
        println(`{x}`);  // 3, 2, 1
    }
}
```

## Implementation Status

- [x] `Iterator` trait definition in prelude
- [x] `IntoIterator` trait definition in prelude
- [x] `ArrayIter<T>` struct
- [x] `impl Iterator for ArrayIter<T>`
- [x] `impl IntoIterator for Array<T>`
- [x] `Array::iter()` method
- [x] Generic associated type resolution bug fixed
- [x] For-of loop generalization (Phase 2)
- [ ] Tuple `IntoIterator` (Phase 3) - compiler magic, separate task
- [x] `FromIterator` trait (Phase 4)
- [x] Iterator combinators (Phase 5) - `map`, `filter`, `fold`, `collect`
- [x] `MapIter<T, U>`, `FilterIter<T>`, `EnumerateIter<T>` structs
- [x] `enumerate()` combinator

### Known Limitations

- **Trait bounds**: `type Iter: Iterator` constraints are not enforced at trait level,
  only checked at usage sites.

- **Closure parameter type inference**: Closures passed to functions require explicit
  parameter type annotations. Type inference from function signature context is not
  yet supported.

  ```wado
  // This doesn't work:
  apply(5, |x| x * 2);  // Error: can't infer type of x

  // This works:
  apply(5, |x: i32| x * 2);  // OK: explicit type annotation
  ```

### TODO

- [ ] **Default trait method implementations**: Once supported, move `collect()` and
      `count()` from each iterator type to the `Iterator` trait as default methods.
      Currently each iterator must implement these methods separately.

- [ ] **`for-of` with `&mut` references**: Support `for-of` on `&mut Iterator` to allow
      cleaner iterator method implementations. Currently iterator methods like `collect()`
      use manual loops because `for-of` copies the iterator (value semantics), preventing
      the original from being exhausted.

## Related WEPs

- [Associated Types](./wep-2026-01-20-associated-types.md) - Foundation for `type Item`
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) - Trait design
- [Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md) - Coercion via `FromIterator`
- [Closure Implementation](./wep-2026-01-16-closure-implementation.md) - Required for combinators

## References

- [Rust Iterator trait](https://doc.rust-lang.org/std/iter/trait.Iterator.html)
- [Rust IntoIterator trait](https://doc.rust-lang.org/std/iter/trait.IntoIterator.html)
- [Rust iter vs into_iter](https://hermanradtke.com/2015/06/22/effectively-using-iterators-in-rust.html)

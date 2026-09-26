# Traits

## Traits

Traits define shared behavior that types can implement. Trait methods use static dispatch: every call is resolved at compile time.

A `trait` and an `interface` are declared in the type namespace: one name reaches one declaration wherever it is written. Neither denotes a type. Each names a set of operations, and no value has one as its type, so a type position naming one is a compile error. A trait reaches a type only as a bound (`fn f<T: Greet>(x: T)`).

```wado
// Trait declaration
trait Greet {
    fn greet(&self) -> String;
}

// Trait implementation
struct Person {
    name: String,
}

impl Greet for Person {
    fn greet(&self) -> String {
        return `Hello, ${self.name}!`;
    }
}

// Usage
let p = Person { name: "Alice" };
println(p.greet());  // "Hello, Alice!"
```

### Supertraits

A trait can require its implementors to implement other traits. `impl Ord for T`
then fails unless `T` also implements `Eq`, and `T: Ord` alone is enough to use
`Eq`'s methods:

```wado
trait Ord: Eq {
    fn cmp(&self, other: &Self) -> Ordering;
}

trait Circle: Shape + Display {
    fn radius(&self) -> i32;
}

// `T: Ord` implies `T: Eq`
fn dedup_sorted<T: Ord>(items: List<T>) -> List<T> { ... }
```

A trait that reaches itself through supertraits is an error. A method name
reachable through more than one of a receiver's bounds is ambiguous at the call
site; name the trait that declares it to resolve it (`Left::name(&x)` — see
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)). The bounds
a body may name that way include the implied ones, so `Eq::eq(&a, &b)` resolves
under `T: Ord`.

### Multiple Traits

A struct can implement multiple traits:

```wado
trait Named {
    fn name(&self) -> String;
}

trait Aged {
    fn age(&self) -> i32;
}

impl Named for Person {
    fn name(&self) -> String { return self.name; }
}

impl Aged for Person {
    fn age(&self) -> i32 { return self.age; }
}
```

### Method Resolution

A call `recv.m(args)` resolves in one order, stated in full by
[WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md). The receiver
decides which step answers:

1. An inherent method (`impl Type { … }`) shadows every trait method of that
   name, along the whole newtype chain.
2. A reference receiver's `&T` impls come before the base type's.
3. The trait impls that apply to the receiver are ranked, below.
4. A receiver whose type is a type parameter answers from its bounds instead:
   the first bound declaring the method, and two or more declaring it is an
   error.

```wado
struct Robot { id: i32 }

// Inherent method
impl Robot {
    fn greet(&self) -> String { return "Beep boop"; }
}

// Trait method (won't be called because inherent method exists)
impl Greet for Robot {
    fn greet(&self) -> String { return "Hello from trait"; }
}

let r = Robot { id: 1 };
r.greet();  // Returns "Beep boop" (inherent method wins)
```

#### Scope

A trait contributes candidates only where its declaration is in scope: declared
in this module, imported by name or alias, re-exported to it through `pub use`,
or one of the prelude's. Importing a type brings none of the traits its impls
mention. A bound is a name like any other, so calling a supertrait's method
through `T: Sub` needs `Base` imported too. This is what keeps a library's new
blanket impl from changing what a call means in a module that never named it.

Not yet enforced for a supertrait's method called through a bound. See
[WEP: Trait Resolution](./wep-2026-09-01-trait-resolution.md#scope-gates-method-calls-not-the-bounds-path).

#### The Order

Several impls applying to one receiver is normal: a trait carries several
blanket impls. They are ranked:

1. A variadic impl (`impl<..T> Tr for [..T]`) yields to a non-variadic one of
   the same trait at the same argument list.
2. The newtype before its base. The search stops at the first level of the
   receiver's newtype chain that answers.
3. Within one level, the impl that names more of the receiver. One written for
   the receiver (`impl Tag for Box_<i32>`) comes first, then one written for its
   head (`impl<T> Tag for Box_<T>`), then a value blanket
   (`impl<T: Bound> Tr for T`). A blanket names no type at all, only a condition
   the receiver meets. See [Specific Impls Win](#specific-impls-win).

Where an impl was written is read at no rank, so a call means the same thing to
every reader. Specificity is not a rank either: generality reads an impl's
target, never its bounds, so `impl<T: A + B>` beside `impl<T: A>` reports rather
than preferring the narrower one.

#### Ambiguity

Candidates the ranks cannot separate are an error. There are two of them,
because the fix differs:

- Two traits declaring the method name. They share no contract, so the call
  names one: `Alpha::describe(&x)`.
- Two impls of one trait, neither written for the receiver. A blanket has no
  name to call it by, so the fix is an `impl Tr for TheType`, which generality
  puts above both.

Wado has no fully qualified `<Type as Trait>::method()` form, because a leading
`<` in expression position begins JSX. A call names its trait with the
trait-qualified form `Trait::method(recv, …)` instead (see
[WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)). An
associated function with no `self` has no receiver argument to bind `Self`
from, so that form cannot name it.

Arguments filter candidates before the ranks run: one trait at several argument
lists is an overload set the call's arguments choose from (see
[One Trait at Two Argument Lists](#one-trait-at-two-argument-lists)). Operators
and indexing select by operand type instead.

### Default Method Implementations

Trait methods can have default implementations. Implementors can override them or use the defaults:

```wado
trait Summary {
    fn title(&self) -> String;  // required - must be provided

    // Default method - uses self.title()
    fn summary(&self) -> String {
        return `Title: ${self.title()}`;
    }
}

struct Article { headline: String }

// Only provides the required method; summary() uses the default
impl Summary for Article {
    fn title(&self) -> String { return self.headline; }
}

struct Report { headline: String, body: String }

// Overrides the default summary()
impl Summary for Report {
    fn title(&self) -> String { return self.headline; }
    fn summary(&self) -> String { return `${self.headline}: ${self.body}`; }
}
```

Default methods can call other trait methods (both required and default), and the calls are resolved against the implementing type.

### Associated Types

Traits can declare associated types - placeholder types that are specified by implementors:

```wado
trait Container {
    type Item;  // Associated type declaration

    fn get(&self) -> Self::Item;
    fn set(&mut self, value: Self::Item);
}

struct IntBox {
    value: i32,
}

impl Container for IntBox {
    type Item = i32;  // Associated type binding

    fn get(&self) -> Self::Item {
        return self.value;
    }

    fn set(&mut self, value: Self::Item) {
        self.value = value;
    }
}
```

Within trait methods and implementations, `Self::TypeName` refers to the associated type. The type is resolved at compile time based on the implementing type.

### Bounded Associated Types

Associated types can have trait bounds that constrain what types can be used as the associated type:

```wado
trait Collection {
    type Element;
    type Builder: CollectionBuilder<Element = Self::Element, Output = Self>;
}
```

Here `Builder` must implement `CollectionBuilder` with matching `Element` and `Output` types. The `Type = ConcreteType` syntax constrains associated types of the bound trait to specific types.

### Blanket Implementations

A blanket impl provides a trait implementation for all types that satisfy a given bound:

```wado
// Any type that builds itself satisfies Collection automatically
impl<T: CollectionBuilder<Output = T>> Collection for T {
    type Element = T::Element;
    type Builder = T;
}
```

This avoids the need for explicit `impl Collection for ...` on every self-building type. `T::Element` names the `Element` that `T`'s `CollectionBuilder` impl binds.

### Impl Type Parameters Are Declared

An `impl` declares its type parameters in `impl<...>`, and that list is the only way to introduce one. A name in the target or the trait reference that the list does not hold is a type, and the module must declare it:

```wado
impl<T> List<T> { ... }                 // inherent
impl<T: Ord> List<T> { ... }            // with a bound
impl<K: Ord, V> TreeMap<K, V> { ... }   // every parameter listed
impl<T> Default for List<T> { ... }     // trait implementation
impl Display for List<i32> { ... }      // one instantiation declares none
```

### Impl Type Parameters Must Be Determined

An `impl`'s target and trait reference between them must name every type parameter it declares. A use site determines them from the receiver and the trait arguments and from nothing else, so one neither mentions has no value to be given:

```wado
impl<A: Eq, T: Eq> Dup for T { ... }  // ERROR: the type parameter `A` is not
                                      //        constrained by the impl target
                                      //        or the trait reference
```

A bound determines one too, through the types it writes:

```wado
// `S` fixes `FieldTypes`, which fixes `..F`
impl<S: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect> Inspect for S { ... }
```

The bound's subject is not itself determined this way: `A: Eq` says what `A` must satisfy, not what `A` is.

### Standard Library Traits

The prelude defines the indexing traits `IndexValue`, `IndexAssign`, `IndexRef`, and `IndexRefMut`, each with an associated `Output` type. See [Indexing Traits](#indexing-traits) for full definitions.

### Trait Bounds

Type parameters can have trait bounds that constrain what types can be used:

```wado
// Struct with trait bound
struct SortedPair<T: Ord> {
    first: T,
    second: T,
}

// Multiple bounds with + syntax
struct PrintableOrd<T: Ord + Printable> {
    value: T,
}

// Bounds on function type parameters
fn max<T: Ord>(a: T, b: T) -> T {
    if a > b { return a; }
    return b;
}

// Bounded impl blocks - methods only available when T: Ord
impl<T: Ord> List<T> {
    pub fn sort(&mut self) { ... }
    pub fn sorted(&self) -> List<T> { ... }
}

// Bounded trait implementations - Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}
```

## Coherence and Orphan Rules

Wado enforces coherence: a `(Trait, Type)` pair is implemented once. A second impl of one pair is rejected where it is written, and the orphan rules below keep two packages from each writing one.

That is a rule about where impls may be written, not about how many apply to a call: a trait carries several blanket impls, and more than one of them can apply to a receiver. [Method Resolution](#method-resolution) orders those.

### Package Boundary

The unit of coherence is a package — all source files compiled together from the same `wado.toml` project. Types and traits are classified relative to that boundary:

| Module source                                       | Classification |
| --------------------------------------------------- | -------------- |
| `./file.wado` (relative path import)                | Local          |
| Entry-point file                                    | Local          |
| A module a Kiln generator produces for this package | Local          |
| A `[dependencies]` package                          | Foreign        |
| `core:*` (standard library)                         | Foreign        |
| `wasi:*` (WASI interfaces)                          | Foreign        |
| A Wasm asset (`with { type: "wasm" }` or `"wat"`)   | Foreign        |
| Remote URL                                          | Foreign        |

### The Orphan Rule

For `impl<P1..Pn> Trait<A1..Am> for T0`, the implementation is valid if and only if at least one of these conditions holds:

1. `Trait` is local (defined in the current package), or
2. The sequence `T0, A1, A2, …, Am` contains a local type at some position `i`, and no uncovered type parameter appears at any position `j < i`.

#### Uncovered type parameter

A type parameter `Pk` is _uncovered_ at position `i` if the type at position `i` is literally `Pk` (bare, not wrapped inside another type constructor). `List<Pk>` is covered; `Pk` alone is uncovered.

#### Fundamental types

`&T` and `&mut T` are _fundamental_ — they are looked through when checking positions. `impl Trait for &LocalType` counts as having `LocalType` at position `T0`.

### Examples

| Implementation                       | Verdict   | Reason                                                     |
| ------------------------------------ | --------- | ---------------------------------------------------------- |
| `impl Eq for MyStruct`               | Allowed   | `MyStruct` is local (T0 is local)                          |
| `impl MyTrait for String`            | Allowed   | `MyTrait` is local                                         |
| `impl<T: Eq> Eq for MyBox<T>`        | Allowed   | `MyBox` is local (T0 is local)                             |
| `impl From<MyError> for String`      | Allowed   | `MyError` (local) at A1, no uncovered param before it      |
| `impl<T> From<MyType<T>> for String` | Allowed   | `MyType` (local head) at A1, no uncovered param before it  |
| `impl<T> From<T> for MyType`         | Allowed   | `MyType` is local at T0, reached before T1=`T`             |
| `impl Eq for String`                 | Forbidden | Both `Eq` and `String` are foreign                         |
| `impl Eq for List<i32>`              | Forbidden | `Eq` foreign, `List` (head of T0) is foreign               |
| `impl<T> Eq for T`                   | Forbidden | T0 is uncovered type parameter, `Eq` is foreign            |
| `impl<T> From<T> for String`         | Forbidden | T0=`String` (foreign), T1=`T` (uncovered) before any local |
| `impl From<String> for i32`          | Forbidden | T0=`i32` (foreign), A1=`String` (foreign), no local found  |

### Rationale

The orphan rule prevents two packages from independently providing `impl Trait for Type` for the same `(Trait, Type)` pair, which would make method resolution ambiguous when both packages are used together. By requiring something local, either the trait or a type the sequence rule reaches, every valid implementation is "owned" by exactly one package.

The sequence rule (RFC 2451 style) allows `impl From<LocalError> for String` — even though `String` is foreign — because `LocalError` appears in the trait's type argument at position A1 with no uncovered type parameter before it. This makes it unnecessary to define a mirror `Into` trait just to work around stricter rules.

### Inherent Impls

An inherent impl (`impl Type { … }`, with no trait) is subject to a simpler
coherence rule: it may only be written in the package that owns the type.
The self type's head constructor must be local.

| Implementation           | Verdict   | Reason                                              |
| ------------------------ | --------- | --------------------------------------------------- |
| `impl MyStruct { … }`    | Allowed   | `MyStruct` is local                                 |
| `impl<T> MyBox<T> { … }` | Allowed   | `MyBox` (head) is local                             |
| `impl i32 { … }`         | Forbidden | `i32` is foreign                                    |
| `impl String { … }`      | Forbidden | `String` is foreign                                 |
| `impl<T> Array<T> { … }` | Forbidden | `Array` is foreign                                  |
| `impl List<u8> { … }`    | Forbidden | `List` (head) is foreign — even when fully concrete |

This mirrors the trait-impl rationale: if two packages could each add inherent
methods to the same foreign type, their methods would collide. To extend a
foreign type from another package, define a local trait and implement it for
that type (`impl MyExt for String`) — the orphan rule above permits this because
the trait is local. The owning package itself (e.g. `core` for `String` /
`Array<T>` / `List<T>`) is of course free to spread inherent impls across its own
modules.

### Specific Impls Win

Two impls of one trait may cover a type when one is written for a single
instantiation and the other is generic over the head:

```wado
impl<T> Tag for Box_<T> { … }         // general
impl Tag for Box_<i32> { … }          // specific — wins for Box_<i32>

impl<..T> Tag for [..T] { … }         // general
impl Tag for [i32, i32] { … }         // specific — wins for [i32, i32]
```

The specific impl applies to the instantiation it names; every other
instantiation takes the general one. Declaration order does not matter. This is
the generality rank of [Method Resolution](#the-order) — the same rank that puts
either of these above a value blanket (`impl<T: Bound> Tag for T`).

This holds only for a **trait** impl, where the trait gives both methods one
signature. An inherent impl carries no such contract, so the pair is rejected:

```wado
impl<T> Box_<T> { fn a(&self) -> String { … } }
impl Box_<i32> { fn a(&self) -> i32 { … } }   // ERROR: duplicate definition of `a`
```

Two impls that are general in the same way cannot be ordered at all, so a second
variadic impl of one trait is rejected where it is written:

```wado
impl<..T: Inspect> Tag for [..T] { … }
impl<..T: Eq> Tag for [..T] { … }     // ERROR: overlapping variadic impls
```

Bounds do not separate them. A trait's own arguments do, since they make the
two impls of different traits:

```wado
impl<..T> Conv<i32> for [..T] { … }    // OK
impl<..T> Conv<String> for [..T] { … } // OK — a different trait
```

### One Trait at Two Argument Lists

A trait may be implemented for one type at several argument lists — each impl
is legal, and the arguments choose between them
([WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md)):

```wado
impl Take<A> for bool { … }
impl Take<B> for bool { … }

f.take(B { v: 1 })          // OK: a named struct literal selects Take<B>
f.take(a)                    // OK: the local's declared type selects Take<A>
```

Any argument whose type the call site fixes selects: a local, a field read, a
call's return type, an operator's result, a cast, an associated constant, an
enum case, a range.

Selection is unique-or-error, with no ranking. An argument whose type the
call site does not pin — above all a bare literal, which could coerce to
several widths — admits every candidate it could coerce to and never selects
one, so a literal-only distinction stays ambiguous:

```wado
impl Take<i32> for bool { … }
impl Take<i64> for bool { … }

f.take(42)                   // ERROR: the arguments do not select
f.take(42 as i64)            // OK: the cast selects Take<i64>
Take::<i64>::take(&f, 42)    // OK: the trait turbofish pins the list
```

This is deliberate: letting the literal's default type decide would make
adding an `impl Take<i32>` silently retarget every existing call that meant
`Take<i64>`. A closure or a compound literal is typed by the parameter it is
passed to, so it carries nothing to select on either, and the error names the
argument that came up empty.

Operators resolve their impl by operand type on the same principle, which is
why `List<T>` implements `IndexValue<i32>`, `IndexValue<RangeExclusive<i32>>`,
and `IndexValue<RangeInclusive<i32>>` at once — and why the same impls answer
the method spelling, `l.index_value(i)`.

A trait's associated function obeys the same rule, selected on its first
argument. It has no receiver to fix `Self`, so the type is written out and the
argument chooses among the impls that declare the function. Rust needs
`<M as Enc<A>>::make` here:

```wado
impl Enc<A> for M { fn make(v: A) -> i32 { … } }
impl Enc<B> for M { fn make(v: B) -> i32 { … } }

M::make(A { })               // selects Enc<A>
M::make(B { })               // selects Enc<B>
```

Two _different_ traits declaring one method name for one receiver is a
separate case and is always reported: name the trait
(`Alpha::describe(&x)`). Argument selection never crosses trait lines —
impls of different traits share no contract.

## Iterator Traits

The prelude defines iterator traits for generic iteration over collections.

### Iterator - Core Iteration Trait

```wado
/// Types that can yield a sequence of values
pub trait Iterator {
    type Item;

    /// Advances the iterator and returns the next value.
    /// Returns None when iteration is complete.
    fn next(&mut self) -> Option<Self::Item>;
}
```

### IntoIterator - Conversion Trait

```wado
/// Types that can be converted into an iterator
pub trait IntoIterator {
    type Item;
    type Iter: Iterator<Item = Self::Item>;

    /// Creates an iterator from a value
    fn into_iter(&self) -> Self::Iter;
}
```

### FromIterator - Collection Construction

```wado
/// Types that can be constructed from an iterator of `Elem`
pub trait FromIterator {
    type Elem;
    fn from_iter<I: Iterator<Item = Self::Elem>>(iter: &mut I) -> Self;
}
```

### SliceValueIter

`SliceValueIter<T>` is the by-value iterator for the whole sequence family: `Array<T>`, `List<T>`, and `Slice<T>` all reach it through `iter_value()`. [The Sequence Family](./wep-2026-06-02-sequence-family.md) owns the `Value` / `Ref` / `RefMut` axis and the rest of the family's iterators.

### Terminals

Everything `Iterator` declares is available on every implementor, adapters included — the terminals bounded by their element type among them (`sum` / `product` need `Item: Add<Output = Item>` / `Mul<Output = Item>`, `min` / `max` need `Item: Ord`). See [`core:prelude`](./stdlib-core-prelude.md) for the full list and each one's behaviour.

### Usage

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
let rest = rest_iter.collect();  // [2, 3, 4, 5]

// Terminals compose with the adapters
let total = arr.iter_value().filter(|x| x % 2 == 1).sum();  // Some(9)
```

### Value Semantics

By-value iteration (`into_iter()`, `iter_value()`, `for let x of list`) returns copies of elements. Reference iteration yields references instead: `iter_ref()` and `for let x of &list` yield `&T`, and `iter_ref_mut()` and `for let x of &mut list` yield `&mut T`.

`&mut` iteration mutates elements in place when the element type has an addressable interior: `struct`, `List`, `String`, `i128`/`u128`. A write through the `&mut T` lands on the element:

```wado
for let p of &mut points {
    p.x += 1;  // mutates the element in place
}
```

A replace-on-assign element type (`primitive`, `enum`, `flags`, `variant`, `fn`) has no addressable interior, so a write through `&mut T` would be lost. `&mut` iteration over such a list is a compile error; use indexed access instead:

```wado
for let mut i = 0; i < arr.len(); i += 1 {
    arr[i] = arr[i] * 2;
}
```

For `primitive`, `enum`, `flags`, and `fn`, nothing survives the copy, so taking `&mut` of a field or element is a compile error outright. That holds whether it is written `&mut x.f` / `&mut xs[i]` or taken implicitly by a `&mut self` receiver. A `&mut` of a _local_ is fine, since it writes to the variable itself.

A `variant` place admits `&mut`. Its payload is shared, so a mutation _through_ it lands, though replacing the whole value does not.

### Custom Iterables

Any type can be made iterable by implementing `IntoIterator`:

```wado
struct Stack<T> { items: List<T> }
struct StackIter<T> { items: List<T>, index: i32 }

impl<T> Iterator for StackIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> { ... }
}

impl<T> IntoIterator for Stack<T> {
    type Item = T;
    type Iter = StackIter<T>;
    fn into_iter(&self) -> StackIter<T> { ... }
}

// Now for-of works
for let x of stack { ... }
```

### Iterator Combinators

Iterators support `map`, `filter`, and `fold` for functional-style data processing:

```wado
let arr: List<i32> = [1, 2, 3, 4, 5];

// map - transform each element
let doubled = arr.into_iter().map(|x| x * 2).collect();
// [2, 4, 6, 8, 10]

// filter - keep elements matching predicate
let evens = arr.into_iter().filter(|x| x % 2 == 0).collect();
// [2, 4]

// fold - reduce to single value
let sum = arr.into_iter().fold(0, |acc, x| acc + x);
// 15

// Chaining combinators
let result = arr.into_iter()
    .filter(|x| x > 2)
    .map(|x| x * 10)
    .collect();
// [30, 40, 50]
```

## Builtin Comparison Traits

The prelude defines traits for comparison operators:

### Eq - Equality

```wado
/// Types that can be compared for equality
pub trait Eq<Rhs = Self> {
    /// Returns true if self equals other
    fn eq(&self, other: &Rhs) -> bool;
}
```

The `==` and `!=` operators use `Eq::eq`:

- `a == b` desugars to `Eq::eq(&a, &b)`
- `a != b` desugars to `!Eq::eq(&a, &b)`

`==` can span two types. The right operand picks among a type's `Eq<Rhs>` impls
exactly as it picks among its `Add<Rhs>` impls, so `StrSlice` and `String`
compare directly, in either order, with nothing copied.

A reference compared with a value asks the impls written for the reference
itself (`impl … for &T`), picked the same way. So a `&String` equals a `String`
through `impl<T: AsStrSlice> Eq<String> for &T`. Nothing dereferences the
reference: no impl answers `&i32 == i32`, so that comparison is an error.

Two references compare the values they point to, as in Rust. The prelude's
`impl<T: Eq> Eq for &T` (and the same for `&mut T`) answers `&a == &b` with
`a == b`, so `&T: Eq` holds exactly when `T: Eq` does. `&mut` coerces to `&`,
so a `&mut` operand compares with a `&` one on either side. To ask whether two references
point to the same value, call `ref_eq` (see
[Reference Identity](./spec-memory.md#reference-identity)).

### Ordering Enum

```wado
/// Result of a three-way comparison
pub enum Ordering {
    Less,    // first value is less than second
    Equal,   // values are equal
    Greater, // first value is greater than second
}
```

### Ord - Ordering

```wado
/// A total order over the type
pub trait Ord: Eq {
    /// Compares self with other and returns an Ordering
    fn cmp(&self, other: &Self) -> Ordering;
}
```

`Ord` is a total order, and `sort`, `TreeMap` and every `T: Ord` bound read it.
On a float it is IEEE 754-2019 `totalOrder`, so it separates `-0.0` from `0.0`
and places each NaN at one end rather than calling it equal to what it met.

On every type but a float, a comparison operator means what `Ord::cmp` answers:

- `a < b` desugars to `Ord::cmp(&a, &b) == Ordering::Less`
- `a > b` desugars to `Ord::cmp(&a, &b) == Ordering::Greater`
- `a <= b` desugars to `Ord::cmp(&a, &b) != Ordering::Greater`
- `a >= b` desugars to `Ord::cmp(&a, &b) != Ordering::Less`

A float is the one type whose operators are not its `Ord`: all four are IEEE,
so a NaN answers false and the two zeroes are one value. This holds for `f16`
and `bf16` as for `f32` and `f64`. One trait cannot carry both orders, because
`Ordering` has three cases and an IEEE comparison has four answers. See
[WEP: The Operator Order and the Total Order](./wep-2026-09-23-comparison-traits.md).

### Default Implementations

`String` and `List<T>` implement `Eq` and `Ord` with lexicographic comparison:

```wado
impl Eq for String { ... }  // byte-by-byte equality
impl Ord for String { ... } // lexicographic ordering

// Usage
let a = "apple";
let b = "banana";
if a < b { ... }  // true
```

## Default Trait

See [WEP: Default Trait](./wep-2026-03-04-default-trait.md).

The prelude defines a `Default` trait providing a uniform "zero value" / "empty value" interface:

```wado
pub trait Default {
    fn default() -> Self;
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

```wado
let n = i32::default();           // 0
let s = String::default();        // ""

fn make_default<T: Default>() -> T { return T::default(); }

let x = make_default::<i32>();              // 0
let arr = make_default::<List<String>>();  // []
```

### Auto-Derivation

`Default` is auto-derived for a non-generic struct when every field has a declared default expression (`f: T = expr`). It is derived where a `S::default()` call, a `T: Default` bound, or an `impl Default for S;` marker needs it, not for every eligible struct. A fieldless struct qualifies, having exactly one value. This is what lets a marker like `NoFields` serve as a type parameter's default. See [Struct Field Defaults](./spec-types.md#struct-field-defaults). A user-written `impl Default for S` overrides the auto-derived one. Generic structs require an explicit impl.

```wado
struct Config {
    host: String = "localhost",
    port: i32 = 8080,
}

let c = Config::default();  // Config { host: "localhost", port: 8080 }
```

For other types, the user writes the impl manually:

```wado
struct Point { x: i32, y: i32 }

impl Default for Point {
    fn default() -> Point { return Point { x: 0, y: 0 }; }
}
```

## String Parsing Traits

Two prelude traits parse a value from text, both taking any `AsStrSlice` and returning `Result`. `FromStr` is strict; `LenientFromStr` is forgiving of human input. `char`, `bool`, the integer types (`i128`/`u128` included), and the float types implement both; `String` implements only the lenient one, since taking a string as itself cannot fail.

```wado
i32::from_str("42")              // Ok(42)
i32::from_str("0x2A")            // Err — strict rejects the prefix

i32::from_str_lenient("0x2A")    // Ok(42)  — radix prefixes 0x/0o/0b
i32::from_str_lenient("1_000")   // Ok(1000) — `_` digit separators
bool::from_str_lenient("TRUE")   // Ok(true) — casing, plus 1/0
f64::from_str_lenient("inf")     // Ok(f64::INFINITY)
i32::from_str_lenient(" 1 ")     // Err — never trims whitespace
```

`FromStr::from_str` takes any `AsStrSlice` — a `StrSlice` among them, so a field is parsed out of a larger buffer with no substring allocation. See [WEP: String Views](./wep-2026-09-13-string-slice.md) and [WEP: Lenient String Parsing](./wep-2026-06-22-lenient-from-str.md).

## Arithmetic Operator Traits

The prelude's binary operator traits (`Add`, `Sub`, `Mul`, `Div`, `Rem`,
`BitAnd`, `BitOr`, `BitXor`) carry a right-hand type parameter defaulting to
`Self`:

```wado
trait Add<Rhs = Self> {
    type Output;
    fn add(&self, rhs: &Rhs) -> Self::Output;
}
```

Omitting the argument is the ordinary case: `impl Add for Meters` adds two
`Meters`. Writing it lets one type be added to another, and the right operand
selects between the impls:

```wado
impl Add for Meters { … }          // Meters + Meters
impl Add<Feet> for Meters { … }    // Meters + Feet

let total = m + f;                 // selects Add<Feet>
```

Selection follows the same unique-or-error rule as a method call's argument
lists (see [One Trait at Two Argument Lists](#one-trait-at-two-argument-lists)).
`Neg` and `BitNot` are unary and take no argument; `Shl` / `Shr` declare
`rhs: u32`.

The compiler supplies these impls for the integers, and for `f32` / `f64`
except `Rem`. `bool` holds one bit, so it gets the bit operators and no shift;
`v128` gets none, its arithmetic being lane-wise and known only to the lane
type's own impl.

An operator yields `Output`, which a widening impl may make another type, so a
generic body folding back into its own parameter pins it:

```wado
fn sum2<T: Add<Output = T>>(a: T, b: T) -> T { return a + b; }
fn scale<T: Mul>(a: T, b: T) -> T::Output { return a * b; }
```

A bound that writes no argument names the declared default. `T: Add` is
`Add<Self>`, which `impl Add for Cm` answers and `impl Add<Inch> for Cm` does
not.

A bound that writes one asks for that argument. `T: Eq<String>` reaches
`impl Eq<String> for StrSlice`. On a `String` receiver it reaches
`impl Eq for String`, whose `Rhs` is the restated `Self`.

An impl writing `Self` as a trait argument says its own target, so
`impl Add<Self> for Feet` and `impl Add<Feet> for Feet` are one impl.

`Self` in a bound names the type the surrounding declaration implements. A
`trait` binds one and an `impl` binds one. A free function binds none, so `Self`
anywhere in a free function's bounds is an error, and the error names the type
parameter to write instead. A `struct` or `variant` declaration binds none
either, so a bound on its own parameter is the same error.

The position makes no difference. A trait argument (`T: Uses<Self::Item>`) and
an associated-type constraint nested under one
(`T: Sink<Cb = fn(Self::Item) -> i32>`) are both rejected. Rust rejects the same
spelling.

The rule is the same wherever a bound is written: on a type parameter, on a
supertrait (`trait AsStrSlice: Eq<String>`), or on an associated type
(`type Item: Eq<String>`). A bound's arguments are spelled where it is written,
so a supertrait clause naming its own trait's parameter — `trait Gauge<X>:
Measure<X>` — supplies `Measure<i32>` under `T: Gauge<i32>`. A position the
clause leaves out takes the declared default there too, so `trait A<T>: B<T>`
over `trait B<X, Y = i32>: C<Y>` supplies `C<i32>`.

`T::Output` under two bounds that both declare `Output` is ambiguous unless
they bind it to the same type.

An operator names these traits by construction, not by spelling: a trait
declared as `Add` elsewhere shadows the name but does not answer `+`.

A parameter with no default is left open by a bound that writes nothing there:
`T: Pick` holds for every `impl Pick<K>`, and the body cannot say which `K`.
`T: Pick<String>` names it, and holds only for `impl Pick<String>`.

Two bounds on one trait are two obligations, each asking for what it writes.
Under `trait Pick<K = i32>`, `T: Pick + Pick<String>` asks for `impl Pick<i32>`
as well as `impl Pick<String>`. A method call on such a parameter reads the
bound that writes arguments. The trait is one either way, so naming it selects
nothing.

An impl and a bound already in scope read a written argument differently.

An impl answers a bound only where every position agrees, each side counting the
trait's declared default where it wrote nothing. `impl Conv<i32> for Holder` does
not answer `U: Conv` when `Conv` declares `X = String`.

A bound already in scope supplies a bare request whatever it writes, and a
supertrait does the same. `T: Conv<i32>` supplies `Conv`, and
`AsStrSlice: Eq<String>` supplies `Eq`. Nothing is chosen at such a request. The
bound is already fixed, and the question is only whether the trait is among what
the parameter carries.

## Indexing Traits

The prelude defines traits for index-based access:

### IndexValue - Value Read

```wado
/// Returns element by value (copy)
pub trait IndexValue<IndexType> {
    type Output;
    fn index_value(&self, index: IndexType) -> Self::Output;
}
```

### IndexAssign - Value Write

```wado
/// Assigns value to element at index
pub trait IndexAssign<IndexType> {
    type Output;
    fn index_assign(&mut self, index: IndexType, value: Self::Output);
}
```

### IndexRef - Reference Read

```wado
/// Returns element by shared reference
pub trait IndexRef<IndexType> {
    type Output: Ref;
    fn index_ref(&self, index: IndexType) -> &Self::Output;
}
```

### IndexRefMut - Mutable Reference

```wado
/// Returns element by mutable reference
pub trait IndexRefMut<IndexType> {
    type Output: RefMut;
    fn index_ref_mut(&mut self, index: IndexType) -> &mut Self::Output;
}
```

### Dispatch

A use site reads the trait that matches what it does with the element:

- A bare read `c[i]` copies the element out through `IndexValue`.
- An assignment `c[i] = v` writes through `IndexAssign`.
- `&c[i]` and a `&self` receiver take `IndexRef` when the container has it, and a copy otherwise.
- A `&mut self` receiver and a field write `c[i].f = v` take `IndexRefMut`. On a container without it they are compile errors.

`Ref` and `RefMut` are sealed marker traits the compiler provides. `Ref` holds for a type whose value `&T` can alias, such as a `struct`, `List`, `String`, tuple, `variant`, `fn`, `i128`/`u128`, or reference. `RefMut` holds for the `Ref` types mutated in place rather than replaced on assignment, which excludes `variant` and `fn`. A scalar, `enum`, `flags`, or `resource` element is neither, so it is read and written by value only.

`List<T>` and `Array<T>` implement `IndexValue` and `IndexAssign` for every element type, `IndexRef` when `T: Ref`, and `IndexRefMut` when `T: RefMut`:

```wado
let mut arr: List<i32> = [1, 2, 3];
let x = arr[0];    // IndexValue::index_value
arr[1] = 100;      // IndexAssign::index_assign
```

See [WEP: Indexing Traits Design](./wep-2026-01-20-indexing-traits.md).

## Serialization and Deserialization

Wado provides a format-agnostic serialization framework via `core:serde` and a JSON implementation via `core:json`. See [WEP: Serialization and Deserialization](./wep-2026-02-28-serde.md).

### Compiler-Synthesized `impl`

The syntax `impl Trait for Type;` (semicolon instead of block) signals that the compiler generates the method body. Supported traits: `From`, `Serialize`, `Deserialize`, `Eq`, `Ord`, `Default`, and `Inspect`. For the structurally-checkable traits (`Eq` / `Ord` / `Default` / serde) the marker is also a conformance check — a compile error at its own span if `Type` is ineligible. An `Inspect` marker always validates. A `Display` marker (`impl Display for Type;`) is rejected — `Display` is not derivable for an arbitrary type; write a real `impl Display { fn fmt … }`, or rely on the automatic enum / newtype `Display`.

```wado
use { Serialize, Deserialize } from "core:serde";

struct User {
    name: String,
    age: i32,
}

impl Serialize for User;      // compiler generates serialize method
impl Deserialize for User;    // compiler generates deserialize method
```

The compiler inspects the type definition (struct, enum, variant, or flags) and synthesizes the appropriate method body. This is a compile error if a field or case's type doesn't implement the required trait.

Deserialization rejects a repeated field or key by default; a format overrides `Deserializer::on_duplicate_key` to be lenient. Nesting past a format's `max_depth` is a `DepthLimitExceeded` error, not a trap.

Struct field names are serialized verbatim by default (identity); see [Serialization Names](./wep-2026-02-28-serde.md#serialization-names) for `name` / `name_policy` overrides.

### Bound-Driven Serialize / Deserialize

The marker above is optional: a `T: Serialize` bound is satisfied structurally once every field or case of `T` satisfies the trait — the same on-demand model `Eq` / `Ord` use (below). This is how an anonymous struct, which has no name for a marker, becomes serializable:

```wado
use { to_string } from "core:json";

struct Point { x: i32, y: i32 }              // no impl marker needed
let json = to_string(&Point { x: 1, y: 2 }); // Ok("{\"x\":1,\"y\":2}")
let anon = to_string(&{ x: 1, y: 2 });        // Ok("{\"x\":1,\"y\":2}") — anonymous struct
```

An anonymous literal may also compose spread bases: `{ ..a, ..b, field: v }`
builds an anonymous struct whose fields are the union of the bases' and explicit
fields, in source order, last contributor winning on a name collision (and its
type). Each base is a struct value, evaluated once. Unlike a named struct's
leading-single `..base`, composition allows spreads in any position and more than
one; a member every one of whose fields is overwritten by a later member is a
dead-write error. See [WEP: Literal Spread](./wep-2026-07-03-literal-spread.md).

```wado
let base = { user_id: 1, ip: "10.0.0.1" };
let event = { ..base, level: "warn" };  // { user_id, ip, level } — auto-Serialize
```

The explicit marker `impl Serialize for T;` still works — write it to force the impl with no bound present, or to attach `#[wire(name_policy = "...")]` customization. Like `Eq` / `Ord`'s marker (below), it is a conformance check: an ineligible field or case is a compile error at the marker's own span. See [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### Bound-Driven Eq / Ord

`Eq` / `Ord` derive the same on-demand way: the impl for `T` is synthesized only where a `==` / `<` call site, a bound, or an explicit marker needs it. It is not synthesized for every declared type.

The explicit marker `impl Eq for T;` / `impl Ord for T;` is a hard guarantee, not just a request: a compile error, with a reason chain, at the marker's own span if any field or case is ineligible:

```wado
struct Handler { cb: fn(i32) -> i32 }

impl Eq for Handler;
// compile error: cannot derive `Eq` for `Handler`: not every field/case implements `Eq`
```

### Format Traits

`${x:?}` / `${x:#?}` (`Inspect`, plainly or indented) work for every type — no bound needed.

`${x}` (`Display`) uses the type's `impl Display`. Primitives, `String`, plain enums (bare case name), and newtypes (inherited from the base type) have one. So do the prelude's sequences, tuples and ranges, where their elements allow it. Any other struct or variant needs a hand-written `impl Display`; otherwise `${x}` is a compile error and `${x:?}` gives its debug form. So `T: Display` certifies a real string representation. For example, `String::push_display` takes any `Display`. `${x:#}` runs the same `Display` with `Formatter.alternate` set; an impl that ignores the flag renders identically.

```wado
fn describe<T>(v: &T) -> String { return `${v:?}`; }         // any type
fn label<T: Display>(v: &T) -> String { return `${v}`; }     // requires a `Display`
```

See [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### JSON Module (`core:json`)

```wado
use { to_string, from_string } from "core:json";

// Serialize to JSON string
let json = to_string::<User>(&user);   // Result<String, SerializeError>

// Deserialize from JSON string
let user = from_string::<User>(json);  // Result<User, DeserializeError>
```

JSON serialization returns `Err` for `NaN` and `Infinity` float values. JSON deserialization returns `Err` for malformed input, missing required fields, type mismatches, and trailing data.

### JSON NSD Module (`core:json_nsd`)

Non-self-describing JSON format. Structs are encoded as positional arrays (field names omitted), unit variants as discriminant integers, and payload variants as `[disc, payload]`.

```wado
use { to_string, from_string } from "core:json_nsd";

// Struct as positional array
let json = to_string::<User>(&user);   // Result: Ok("[\"Alice\",30]")

// Deserialize from positional array
let user = from_string::<User>(`["Alice",30]`);  // Result<User, DeserializeError>
```

The same `Serialize` and `Deserialize` trait impls work with both `core:json` and `core:json_nsd`.

### Command-Line Arguments (`core:args`)

`core:args` is a non-self-describing, parse-only `Deserializer` over `argv`. Argument types are ordinary structs with `impl Deserialize for T;`: fields become `--long` options, and fields marked `#[wire(positional)]` are filled from bare tokens in declaration order (required, optional, or variadic). Scalar tokens are converted with `LenientFromStr`. See [WEP: Command-Line Argument Parsing](./wep-2026-06-22-core-args.md).

```wado
use { parse } from "core:args";
use { Deserialize } from "core:serde";

struct Cli {
    #[wire(positional)] input: String,
    jobs: i32 = 1,
    verbose: bool = false,
}
impl Deserialize for Cli;

let cli = parse::<Cli>(["in.txt", "--jobs", "4", "--verbose"]);
```

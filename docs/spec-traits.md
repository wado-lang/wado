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
site; name the trait that declares it to resolve it (`Left::name(&x)`, a
[qualified call](#qualified-calls)). The bounds a body may name that way include
the implied ones, so `Eq::eq(&a, &b)` resolves under `T: Ord`.

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

A call `recv.m(args)` resolves in one order. The receiver decides which step
answers:

1. An inherent method (`impl Type { … }`) shadows every trait method of that
   name, along the whole newtype chain.
2. A reference receiver's concrete `&T` impls come before its pointee's.
3. The trait impls that apply to the receiver are ranked, below.
4. A receiver whose type is a type parameter answers from its bounds instead.
   A method that two or more of them declare is an error.

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

A call in a generic body is selected again at each instance. The body is checked
against the impl selected in the generic body itself, and an instance with an
impl written for it takes that impl instead. This holds for a method call and a static call
alike.

#### Scope

A trait contributes candidates only where its declaration is in scope: declared
in this module, imported by name or alias, or re-exported to it through
`pub use`. The prelude's traits are the only ones in scope everywhere. Importing
a type brings none of the traits its impls mention. A bound is a name like any
other, so calling a supertrait's method through `T: Sub` needs `Base` imported
too. This is what keeps a library's new blanket impl from changing what a call
means in a module that never named it.

An impl is visible everywhere, so one in a module the caller never named still
answers once its trait is in scope. Two same-named traits declared in different
modules are distinct traits: each module's call reaches the one it imported, and
a module importing both has the two-trait ambiguity below.

A call whose only candidates come from traits out of scope is an error that
names the trait and the import that would enable it:

```text
no method 'shout' on 'String' in scope: 'Loud' declares it and is not
imported here; add `use { Loud } from "./lib_a.wado"`
```

A supertrait's method called through a bound is gated the same way: `T: Sub`
reaches `Base`'s methods only where `Base` is imported.

#### Candidates

A call's candidates come from two places:

- The impls whose target reaches the receiver anywhere along its newtype chain:
  one written for the receiver's own type, for one instantiation of it
  (`impl Tag for Box_<i32>`), for some of its instantiations
  (`impl<T> Tag for Pair<T, i32>`), or for its head (`impl<T> Tag for Box_<T>`).
- Every value blanket (`impl<T: Bound> Tr for T`) whose bounds the receiver
  satisfies.

A target reaches a receiver when every position it pins holds the receiver's
argument there. A type parameter stands for one type wherever the target writes
it, at any depth. So `impl<T> Tag for Pair<List<T>, i32>` reaches
`Pair<List<u8>, i32>`, and `impl<T> Tag for Pair<T, T>` does not reach
`Pair<i32, i64>`.

A value blanket must bound its receiver parameter. An unbounded
`impl<T> Tr for T` names no condition that could select it, so it is rejected
where it is written.

`()` is the unit type, not the empty tuple `[]`, so an impl for `[..T]` never
reaches it. An anonymous struct is a shape no impl can name, so only a value
blanket reaches it.

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

A candidate's level is the level it is selected at. An impl whose target is the
newtype sits at the newtype's level and one targeting the base at the base's. A
blanket sits at the level where its bounds hold. A newtype inherits its base's
impls, so a blanket whose bound only the base satisfies is still a candidate for
the newtype, at the base's level. This holds when the bound is met through
another blanket too: if that blanket's own bound holds only at the base, both
sit at the base's level.

A reference does not interrupt the chain. A call on `&W`, where `W` is a newtype
over `Inner`, visits `&W`, then `W`, then `&Inner`, then `Inner`. Within one
level the reference precedes its pointee.

How many positions a head impl pins is not part of its generality, so
`impl<T> Tr for Pair<T, i32>` and `impl<A, B> Tr for Pair<A, B>` tie at
`Pair<String, i32>`.

Where an impl was written is read at no rank, so a call means the same thing to
every reader. Specificity is not a rank either: generality reads an impl's
target, never its bounds, so `impl<T: A + B>` beside `impl<T: A>`, or
`impl<T: Ord>` beside `impl<T: Eq>`, reports rather than preferring the narrower
one.

#### Ambiguity

Candidates the ranks cannot separate are an error, reported at the call. There
are three shapes, because the fix differs:

- Two traits declaring the method name. They share no contract, so the call
  names one: `Alpha::describe(&x)`. Blankets of two traits join this collision
  like any other candidate.
- Two blankets of one trait, the receiver satisfying both bounds. A blanket has
  no name to call it by, so the fix is an `impl Tr for TheType`, which
  generality puts above both.
- Two head impls of one trait that both reach the receiver. Neither can be named
  at the call either, and the fix is the same.

```text
ambiguous blanket impls of 'Describe' for 'Point': 'T: Limit' and
'T: ReflectStruct' apply, and nothing ranks them;
write 'impl Describe for Point'
```

Overlapping blankets are not rejected where they are written. Whether two bounds
can both hold is not decidable there, since another module may implement either
bound for a new type at any time. So overriding a library's blanket with a
second blanket of your own reports at every type satisfying both bounds. Write
`impl Tr for YourType` instead.

Arguments filter candidates before the ranks run: one trait at several argument
lists is an overload set the call's arguments choose from (see
[One Trait at Two Argument Lists](#one-trait-at-two-argument-lists)).

#### Eligibility

A bound that does not hold produces no candidate. An impl's bound on its own
parameter is read the same way wherever the impl puts that parameter: in a type
argument (`impl<T: B> Tr for List<T>`), in a pointee (`impl<T: B> Tr for &T`),
or in a pack's elements (`impl<..T: B> Tr for [..T]`).

A rigid type parameter satisfies a bound from the bounds in force on it and from
nothing else. So `[..T]: Ord` does not hold of `[A, B]` under
`A: Inspect, B: Inspect`, and the body that wants it writes `A: Ord, B: Ord`.

A `Reflect*` bound holds only where every member of the receiver is visible at
the use site.

A blanket's receiver parameter is matched by position, not by spelling: a method
parameter named `T` inside the method is the method's own `T`.

#### Qualified Calls

Wado has no fully qualified `<Type as Trait>::method()` form, because a leading
`<` in expression position begins JSX. A call names its trait with the
trait-qualified form `Trait::method(recv, args…)` instead:

```wado
Display::fmt(&p, f);          // p implements two traits declaring `fmt`
Base::name(&x);               // supertrait diamond inside a generic body
Take::<A>::take(&f, B { … }); // one trait's argument list, pinned
```

The receiver is the first argument, spelled to match the method's `self` mode:
`&x` for `&self`, `&mut x` for `&mut self`, the value for `self`. A mismatched
mode is an error, not a coercion, since a value passed to a `&mut self` method
would mutate a copy and drop the change. The one exception is the language's one
reference coercion: `&mut x` also answers a `&self` method. Trailing default
arguments may be omitted, as in the method form.

The receiver supplies `Self`, so a turbofish on the trait carries only the
trait's own arguments. It pins one argument list of an overload set. Without
one, the call's arguments select within the named trait as a method call's do.

Only the named declaration answers. An auto-derived `Eq` answers `Eq::eq`, never
a user trait that happens to declare `eq`. In a generic body the named trait must
be among the receiver's bounds, implied ones included, and a same-named trait of
another module does not answer for it.

A qualified call selects the impl the method form would:
`IntoIterator::into_iter(&list)` takes `impl IntoIterator for &List<T>`, as
`(&list).into_iter()` does.

`Type::method(recv, args…)` names the type's own method instead. There, as in
the method form, an inherent declaration shadows a trait impl's declaration of
the same name when both take `self` or both do not. The trait's is reached as
`Trait::method(recv, …)`.

An associated function with no `self` has no receiver argument to bind `Self`
from, so the trait-qualified form cannot name it.

Rationale: [WEP: Trait Resolution — One Order, Written Down](./wep-2026-09-01-trait-resolution.md).

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

Traits can declare associated types: placeholder types that implementors specify:

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

### Bounds on Generic Traits

A bound that writes no argument names the declared default. `T: Add` is
`Add<Self>` (see [Arithmetic Operator Traits](#arithmetic-operator-traits)),
which `impl Add for Cm` answers and `impl Add<Inch> for Cm` does not. A
parameter with no default is left open by a bound that writes nothing there:
`T: Pick` holds for every `impl Pick<K>`, and the body cannot say which `K`.
`T: Pick<String>` names it, and holds only for `impl Pick<String>`.

A bound that writes an argument asks for that argument. `T: Eq<String>` reaches
`impl Eq<String> for StrSlice`. On a `String` receiver it reaches
`impl Eq for String`, whose `Rhs` is the restated `Self`.

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

An impl writing `Self` as a trait argument says its own target, so
`impl Add<Self> for Feet` and `impl Add<Feet> for Feet` are one impl.

`Self` in a bound names the type the surrounding declaration implements. A
`trait` binds one and an `impl` binds one. A free function binds none, so `Self`
anywhere in a free function's bounds is an error, and the error names the type
parameter to write instead. A `struct` or `variant` declaration binds none
either, so a bound on its own parameter is the same error. The position in the
bound makes no difference: a trait argument (`T: Uses<Self::Item>`) and an
associated-type constraint nested under one
(`T: Sink<Cb = fn(Self::Item) -> i32>`) are both rejected. Rust rejects the same
spelling.

These rules hold wherever a bound is written: on a type parameter, on a
supertrait (`trait AsStrSlice: Eq<String>`), or on an associated type
(`type Item: Eq<String>`). A bound's arguments are spelled where it is written.
So the supertrait clause of `trait Gauge<X>: Measure<X>` names the trait's own
parameter, and supplies `Measure<i32>` under `T: Gauge<i32>`. A position the
clause leaves out takes the declared default there too, so `trait A<T>: B<T>`
over `trait B<X, Y = i32>: C<Y>` supplies `C<i32>`.

A bound's arguments are written in the body's own parameter space, so what they
name is settled at each instantiation. `T: Make<U>` reaches
`impl Make<String>` once the call settles `U` to `String`, and
`T: Make<T::Base>` reaches it once `T` is a type whose `Base` is `String`. A
projection nested inside an argument is answered the same way:
`Make<List<T::Base>>` reaches `impl Make<List<String>>`. A call through the
bound reaches the impl that satisfied the bound.

A bound may pin an associated type (`T: Mul<Output = T>`). The impl that answers
it must bind that type as the pin says.

## Coherence and Orphan Rules

Wado enforces coherence: a `(Trait, Type)` pair is implemented once. A second impl of one pair is rejected where it is written, and the orphan rules below keep two packages from each writing one.

That is a rule about where impls may be written, not about how many apply to a call: a trait carries several blanket impls, and more than one of them can apply to a receiver. [Method Resolution](#method-resolution) orders those.

### Package Boundary

The unit of coherence is a package: all source files compiled together from the same `wado.toml` project. Types and traits are classified relative to that boundary:

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

`&T` and `&mut T` are _fundamental_: they are looked through when checking positions. `impl Trait for &LocalType` counts as having `LocalType` at position `T0`.

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

The sequence rule, in the style of RFC 2451, allows `impl From<LocalError> for String` although `String` is foreign. `LocalError` appears in the trait's type argument at position A1, with no uncovered type parameter before it. This makes it unnecessary to define a mirror `Into` trait just to work around stricter rules.

### Inherent Impls

An inherent impl (`impl Type { … }`, with no trait) is subject to a simpler
coherence rule: it may only be written in the package that owns the type.
The self type's head constructor must be local.

| Implementation           | Verdict   | Reason                                             |
| ------------------------ | --------- | -------------------------------------------------- |
| `impl MyStruct { … }`    | Allowed   | `MyStruct` is local                                |
| `impl<T> MyBox<T> { … }` | Allowed   | `MyBox` (head) is local                            |
| `impl i32 { … }`         | Forbidden | `i32` is foreign                                   |
| `impl String { … }`      | Forbidden | `String` is foreign                                |
| `impl<T> Array<T> { … }` | Forbidden | `Array` is foreign                                 |
| `impl List<u8> { … }`    | Forbidden | `List` (head) is foreign, even when fully concrete |

This mirrors the trait-impl rationale: if two packages could each add inherent
methods to the same foreign type, their methods would collide. To extend a
foreign type from another package, define a local trait and implement it for
that type (`impl MyExt for String`). The orphan rule above permits this because
the trait is local. The owning package may spread inherent impls across its own
modules, as `core` does for `String`, `Array<T>` and `List<T>`.

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
the generality rank of [The Order](#the-order), which also puts either of these
above a value blanket (`impl<T: Bound> Tag for T`).

This holds only for a trait impl, where the trait gives both methods one
signature. An inherent impl carries no such contract, so two inherent blocks
that reach a common receiver may not both define one method name:

```wado
impl<T> Box_<T> { fn a(&self) -> String { … } }
impl Box_<i32> { fn a(&self) -> i32 { … } }   // ERROR: duplicate definition of `a`
```

Inherent blocks that reach no receiver in common may share a name.

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

Wado has no function overloading. Declaring a second one of these under one name
is an error: a free function, an inherent method of one receiver (see
[Specific Impls Win](#specific-impls-win)), a method within one trait
declaration. Arity never tells two apart; default arguments cover optional
parameters.

The one overload set is one trait implemented for one type at several argument
lists. Each impl is legal, and the call's arguments choose between them:

```wado
impl Take<A> for bool { … }
impl Take<B> for bool { … }

f.take(B { v: 1 })          // OK: a named struct literal selects Take<B>
f.take(a)                    // OK: the local's declared type selects Take<A>
```

A trait is identified by its declaration, never by its spelling: an alias names
the trait it aliases, and two modules' same-named traits stay two traits. An
argument list is identified by its resolved types, so `Take<Alias>` and
`Take<A>` name one list. Every member of the set shares one declaration, so all
take the same number of arguments.

Any argument whose type the call site fixes selects: a local, a field read, a
call's return type, a method call's return type, an operator's result, a cast,
an associated constant, an enum case. `if` and `match` select by what their
branches agree on, and a block by its trailing statement.

A range, or a named literal of a generic struct, selects by its head type alone.
`0..<n` is a `RangeExclusive` whatever its endpoints are, which is enough to tell
`IndexValue<i32>` from `IndexValue<RangeExclusive<i32>>`. Two impls sharing that
head, such as `Take<RangeExclusive<i32>>` beside `Take<RangeExclusive<i64>>`,
stay ambiguous for such an argument.

These carry nothing to select on and admit every candidate: a closure, a
compound literal (a tuple, an anonymous struct, a list, a spread), `?`,
`resume`, a tuple comprehension, a labeled block, a tagged template, a static
call `Type::f(…)`, and a function named as a value. A name the argument binds
for itself (a block's `let`, a match arm's pattern) is read the same way.

Selection is unique-or-error, with no ranking. An argument whose type the call
site does not pin admits every candidate it could coerce to and never selects
one. A bare literal is the main case, since it could coerce to several widths,
so a literal-only distinction stays ambiguous:

```wado
impl Take<i32> for bool { … }
impl Take<i64> for bool { … }

f.take(42)                   // ERROR: the arguments do not select
f.take(42 as i64)            // OK: the cast selects Take<i64>
Take::<i64>::take(&f, 42)    // OK: the trait turbofish pins the list
```

This is deliberate: letting the literal's default type decide would make
adding an `impl Take<i32>` silently retarget every existing call that meant
`Take<i64>`. The error names each argument that came up empty and why, so it
says whether an annotation would fix the call.

Survivors naming one trait instantiation are ranked as any candidates are, which
is where [Specific Impls Win](#specific-impls-win) applies. Survivors naming
several instantiations are ambiguous; there is no best match, only a unique one.
Once one impl is selected, the arguments are typed against its signature as for
any call, so a literal coerces to the chosen parameter type and its range is
checked there. Selection never reads a literal's value, and never reads effects:
the chosen method's `with` clause is checked afterwards.

No survivor is a different error from ambiguity. Every argument was admitted by
what it could be, so no candidate admitting it means no impl accepts the
arguments, and the error lists what the overload set does take:

```text
no overload of 'take' accepts these arguments: the candidates are 'Take<i32>'
and 'Take<String>'
```

A newtype receiver inherits its base's impls as candidates, and the same
selection applies to them.

Operators and indexing resolve their impl by operand type on the same principle.
That is why `List<T>` implements `IndexValue<i32>`,
`IndexValue<RangeExclusive<i32>>`, and `IndexValue<RangeInclusive<i32>>` at
once, and why the same impls answer the method spelling, `l.index_value(i)`.

A trait's associated function obeys the same rule. It has no receiver to fix
`Self`, so the type is written out, and the arguments choose among the impls
that declare the function, each read against the parameter written for it. Rust
needs `<M as Enc<A>>::make` here:

```wado
impl Enc<A> for M { fn make(v: A) -> i32 { … } }
impl Enc<B> for M { fn make(v: B) -> i32 { … } }

M::make(A { })               // selects Enc<A>
M::make(B { })               // selects Enc<B>
```

`Type::from(x)` and `Type::try_from(x)` select this way among the type's `From`
and `TryFrom` impls, before `x` is typed. One admitted impl supplies the
argument's expected type, so `Wrapper::from(42)` beside `From<String>` and
`From<i64>` takes `From<i64>` and types `42` as `i64`. Several admitted impls
are ambiguous, and since `from` has no `self` the fix is a cast on the argument:

```wado
impl From<i32> for Wrapper { … }
impl From<i64> for Wrapper { … }

Wrapper::from(42)            // ERROR: a literal argument admits 'i32' and 'i64'
Wrapper::from(42 as i64)     // OK
```

An integer literal coerces to an integer newtype, so `From<i64>` beside
`From<Meters>` (`type Meters = i64`) is ambiguous for it rather than silently
primitive. An argument known only by its head does not preselect: several
same-head impls answering it is the expected reading, and typing the argument
settles it. An inherent static `from` beside `From` impls is the type's own
function and answers without selection.

Argument selection never crosses trait lines, since impls of different traits
share no contract. Two traits declaring one method name for one receiver are
the two-trait [ambiguity](#ambiguity), whatever the arguments.

Rationale: [WEP: Overload Resolution](./wep-2026-07-31-overload-resolution.md).

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

`TreeMap` and `TreeSet` carry the same axis. A map projection needs no suffix,
since `keys()` already names what it yields, and it yields references:

| Type                            | Item       | Reached by               |
| ------------------------------- | ---------- | ------------------------ |
| `TreeSetRefIter<T>`             | `&T`       | `iter_ref()`             |
| `TreeSetValueIter<T>`           | `T`        | `iter_value()`           |
| `TreeMapKeysRefIter<K, V>`      | `&K`       | `keys()`                 |
| `TreeMapKeysValueIter<K, V>`    | `K`        | `keys().iter_value()`    |
| `TreeMapValuesRefIter<K, V>`    | `&V`       | `values()`               |
| `TreeMapValuesValueIter<K, V>`  | `V`        | `values().iter_value()`  |
| `TreeMapEntriesRefIter<K, V>`   | `[&K, &V]` | `entries()`              |
| `TreeMapEntriesValueIter<K, V>` | `[K, V]`   | `entries().iter_value()` |

A map offers no `&mut` traversal: a `&mut` key would break the ordering, and a
`&mut` value buys nothing over `m[k] = v`. The map and set iterators refer to
the collection rather than copying it, so inserting or removing mid-traversal
can skip or repeat an entry. `iter_value().collect()` takes a snapshot.

### Terminals

Everything `Iterator` declares is available on every implementor, adapters included. That covers the terminals bounded by their element type: `sum` / `product` need `Item: Add<Output = Item>` / `Mul<Output = Item>`, and `min` / `max` need `Item: Ord`. See [`core:prelude`](./stdlib-core-prelude.md) for the full list and each one's behaviour.

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

`&mut` iteration mutates elements in place. It needs an element type that is `RefMut` (see [Dispatch](#dispatch)), such as a `struct`, `List`, `String`, or `i128`/`u128`. A write through the `&mut T` lands on the element:

```wado
for let p of &mut points {
    p.x += 1;  // mutates the element in place
}
```

An element type that is replaced on assignment (a primitive, `enum`, `flags`, `variant`, or `fn`) is not `RefMut`, so a write through `&mut T` would be lost. `&mut` iteration over such a list is a compile error; use indexed access instead:

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

## The Sequence Family

Three prelude types hold contiguous sequences:

|                | Fixed length | Growable  |
| -------------- | ------------ | --------- |
| Owned          | `Array<T>`   | `List<T>` |
| Reference view | `Slice<T>`   | —         |

`Slice<T>` is the read-only vocabulary type. An algorithm that only reads is
written once against a slice, and the owned types reach it through
`as_slice()`.

A conversion's name says what it costs: `as_*` returns a view of the same
elements, and `to_*` copies them.

| From → To                  | Method       | Elements copied       |
| -------------------------- | ------------ | --------------------- |
| `Array` / `List` → `Slice` | `as_slice()` | none                  |
| `Slice` → `Array` / `List` | `to_*()`     | all                   |
| `List` → `Array`           | `to_array()` | all, sized to `len()` |

Indexing by a range (`xs[1..<3]`, `xs[1..=2]`) yields a `Slice<T>`, as
`slice(start, end)` does. Both clamp the range to the sequence.

### Sequence and AsSlice

Two prelude traits carry the family's shared methods, split by whether the
implementor has a contiguous backing. All three types implement both.

```wado
pub trait Sequence {
    type Elem;

    fn len(&self) -> i32;
    fn get_unchecked(&self, index: i32) -> Self::Elem;

    // default bodies, written against `len` and `get_unchecked`
    fn is_empty(&self) -> bool;
    fn get(&self, index: i32) -> Option<Self::Elem>;
    fn first(&self) -> Option<Self::Elem>;
    fn last(&self) -> Option<Self::Elem>;
    fn position(&self, pred: fn mut(Self::Elem) -> bool) -> Option<i32>;
}

pub trait AsSlice: Sequence {
    fn as_slice(&self) -> Slice<Self::Elem>;

    // default bodies, through `as_slice`
    fn slice(&self, start: i32, end: i32) -> Slice<Self::Elem>;
    fn iter_value(&self) -> SliceValueIter<Self::Elem>;
    fn iter_ref(&self) -> SliceRefIter<Self::Elem>;
    fn windows(&self, size: i32) -> SliceWindows<Self::Elem>;
    fn chunks(&self, size: i32) -> SliceChunks<Self::Elem>;
}
```

The element type is an associated type, so a bound reads
`S: Sequence<Elem = i32>`. A method that needs a bound on the element, such as
`contains` (`Elem: Eq`), is not a trait method: each type carries it as a
bounded inherent method. Mutation is in neither trait, since a `Slice` has no
mutable backing and length changes belong to `List` alone. `String` implements
neither; its bytes are viewed through `AsByteSlice`, and its text through
[`AsStrSlice`](#string-views).

A function that reads any of the three takes the trait by value:

```wado
fn total<S: AsSlice<Elem = i32>>(xs: S) -> i32 {
    return xs.iter_value().fold(0, |acc, x| acc + x);
}
```

### Slice Semantics

A slice refers to the whole backing array plus a start and an end. It is an
ordinary value: assigning one copies those three fields and never the elements.
A view reads its elements but never hands out `&mut` into the array, so nothing
writes through it (see [Dispatch](#dispatch)). Two consequences follow, both
memory-safe:

- Snapshot. A view keeps referring to the buffer it was created from, so a
  source `List` that grows and reallocates is not observed.
- Aliasing. A write to the source that does not reallocate is visible through
  the view.

A slice compares, orders, displays, and inspects by its elements, not by the
buffer it refers to.

### Bounds Checks

`get(i)` returns `Option<T>` on all three types. `xs[i]` traps when `i` is
outside `0..<len()`, and the trap is the whole contract: its message is
implementation-defined. On a slice this includes a negative index, although the
backing array holds an element there: a view never reads outside itself.

`get_unchecked(i)` leaves the check to the caller, who must guarantee
`0 <= i < len()`. Violating that yields an unspecified value of the element type
or traps. It is never undefined behavior and never compromises memory safety,
because every element read is bounds-checked by the Wasm engine. Wado's
`_unchecked` elides a semantic check, not a memory check, which is why Wado
needs no `unsafe`.

Rationale: [WEP: The Sequence Family](./wep-2026-06-02-sequence-family.md).

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
`a == b`, so `&T: Eq` holds wherever `T: Eq` does. `&mut` coerces to `&`, so a
`&mut` operand compares with a `&` one on either side. To ask whether two
references point to one place, call `ref_eq` (see
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
`Ordering` has three cases and an IEEE comparison has four answers.

A float still implements `Ord`, so a `List<f32>` sorts, and a struct holding a
float derives `Ord` and can key a `TreeMap`.

Written at a concrete float type, the operators are IEEE. Written in a body
generic over `T: Ord`, they read `Ord::cmp`, so the same expression answers
differently at `T = f32`.

Rationale: [WEP: The Operator Order and the Total Order](./wep-2026-09-23-comparison-traits.md).

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

`Default` is derived for a non-generic struct whose every field declares a default expression (`f: T = expr`; see [Struct Field Defaults](./spec-types.md#struct-field-defaults)). A fieldless struct qualifies, having exactly one value. This is what lets a marker like `NoFields` serve as a type parameter's default. A generic struct derives no `Default`, since a default expression is checked against the declaration and not against an instantiation, so it needs a written impl. [Derivation Policy](#derivation-policy) says where the impl is derived, and that a written one wins.

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

A `StrSlice` is an `AsStrSlice`, so a field is parsed out of a larger buffer with no substring allocation (see [String Views](#string-views)).

```wado
pub trait FromStr {
    type Err: Error;
    fn from_str<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
}

pub trait LenientFromStr {
    type Err: Error;
    fn from_str_lenient<S: AsStrSlice>(s: S) -> Result<Self, Self::Err>;
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

It never touches whitespace. Surrounding whitespace is `Err`, and trimming is
the caller's choice. So a value whose whitespace is significant, a `char` `' '`
or an indented `String`, survives.

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

## String Views

`StrSlice` is a prelude type: a view of part of a string, its ends on UTF-8
character boundaries. Creating one copies nothing, and `to_string()` is where a
copy is made. It is an ordinary library type; nothing in the language treats
the name specially.

```wado
let v = "banana".as_str_slice();
let part = v.slice(1, 4);        // "ana"; panics off a character boundary
part.len();                      // 3, in bytes
part.to_string();                // copies out, here and only here
```

`slice(start, end)` takes byte offsets from the view's start. It traps when the
range is out of bounds or either end is not a character boundary.
`slice_unchecked` leaves both checks to the caller. A view refers to its
string's bytes as a [slice](#slice-semantics) does, with the same snapshot and
aliasing behavior.

`AsStrSlice` is the conversion that lets one signature take an owned `String`, a
reference to one, or a view of one:

```wado
pub trait AsStrSlice: Eq<String> {
    fn as_str_slice(&self) -> StrSlice;
    // default bodies over the view: len, slice, chars, starts_with, find, …
}
```

`String` and `StrSlice` implement it, and `impl<T: AsStrSlice> AsStrSlice for &T`
passes a reference through. A parameter that only reads its text names
`AsStrSlice` and takes it by value, so a call site passes a literal bare:
`f("banana")`.

`AsStrSlice` requires `Eq<String>`, so a body generic over it compares its text
with `==` against a string, and a string-literal pattern matches it. `==` on a
type parameter reads the parameter's bounds, so without the requirement neither
would resolve. `StrSlice` and `String` compare in either order, and two views
compare and order by their text.

`StrSlice` carries the search and split methods (`contains`, `starts_with`,
`find`, `split`, `split_once`, the trims, the `strip_*` family), and `String`'s
own delegate to it. A method that answers with part of its input returns a view
of it, so working on part of a string does not copy it out.

Rationale: [WEP: String Views](./wep-2026-09-13-string-slice.md).

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
`v128` gets none, since its arithmetic is lane-wise and only a lane type's own
impl knows it.

An operator yields `Output`, which a widening impl may make another type, so a
generic body folding back into its own parameter pins it:

```wado
fn sum2<T: Add<Output = T>>(a: T, b: T) -> T { return a + b; }
fn scale<T: Mul>(a: T, b: T) -> T::Output { return a * b; }
```

`T::Output` under two bounds that both declare `Output` is ambiguous unless
they bind it to the same type.

An operator names these traits by construction, not by spelling: a trait
declared as `Add` elsewhere shadows the name but does not answer `+`.

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
`Slice<T>` (see [The Sequence Family](#the-sequence-family)); `TreeMap` by `K`.
A slice is a shared view, so it has nothing to write through.

```wado
let mut arr: List<i32> = [1, 2, 3];
let x = arr[0];    // IndexValue::index_value
arr[1] = 100;      // IndexAssign::index_assign
```

Rationale: [WEP: Indexing Traits Design](./wep-2026-01-20-indexing-traits.md).

## Trait Derivation

The compiler writes some trait impls itself. This section says which, and when.
[Serialization](./spec-serialization.md) covers what the derived `Serialize` and
`Deserialize` impls write.

### Derivation Policy

Each prelude trait follows one policy for when an impl exists:

| Policy    | `T: Trait` holds when                                                             | Traits                                             |
| --------- | --------------------------------------------------------------------------------- | -------------------------------------------------- |
| on demand | every member of `T` satisfies the trait                                           | `Eq`, `Ord`, `Default`, `Serialize`, `Deserialize` |
| total     | always                                                                            | `Inspect`                                          |
| written   | an impl is written, `T` is a plain `enum`, or `T` is a newtype whose base has one | `Display`                                          |
| explicit  | an impl is written                                                                | every user-defined trait                           |

`Default` is the one on-demand trait with a condition of its own: every field
carries a default expression, rather than every member satisfying `Default`
(see [Auto-Derivation](#auto-derivation)).

An on-demand impl is generated only where a use needs it, not for every declared
type. For `Eq` and `Ord` that use is an operator, a comparison method, or a
bound; for `Default`, a `T: Default` bound or a `T::default()` call; for serde,
a bound. A [marker](#compiler-synthesized-impl) needs one too.

A `fn`-typed member blocks `Eq`, `Ord`, and serde. A plain `enum` and a `flags`
type have no members, so they satisfy every structural obligation: `Eq` and
`Ord` compare the discriminant or the bitmask.

A generic declaration derives once, for every instantiation whose type
arguments satisfy the trait: `Pair<T>` is `Eq` where `T: Eq`.

A written `impl Trait for T { … }` wins over a derived one for every kind of
type, an `enum`, a `flags` type, and a newtype included. It wins only for the
instances it reaches. `impl<T> Eq for Pair<T, i32>` answers for
`Pair<String, i32>`, and `Pair<i32, i64>` still derives. An impl reaches every
instance of its head only when its target writes each argument as a distinct
type parameter. A concrete argument or a repeated parameter narrows it to some
instances, and a reference, a tuple, `()`, or a function type is one shape at a
time.

`Inspect` holds for every type, a type parameter included. A user-defined trait
is never derived.

### Compiler-Synthesized `impl`

A marker `impl Trait for Type;` (a semicolon instead of a block) asks the
compiler to write the impl's methods. It is accepted for `From`, `Serialize`,
`Deserialize`, `Eq`, `Ord`, `Default`, and `Inspect`, on a struct, enum,
variant, or flags type.

```wado
use { Serialize, Deserialize } from "core:serde";

struct User {
    name: String,
    age: i32,
}

impl Serialize for User;      // compiler generates serialize method
impl Deserialize for User;    // compiler generates deserialize method
```

For `Eq`, `Ord`, `Default`, and serde, a marker is also a conformance check. A
type that is not eligible is a compile error at the marker's own span, with a
reason chain:

```wado
struct Handler { cb: fn(i32) -> i32 }

impl Eq for Handler;
// compile error: cannot derive `Eq` for `Handler`: not every field/case implements `Eq`
```

A bound that does not hold is only unsatisfied where it is asked; only a marker
fails at its own span. An `Inspect` marker always passes.

A `Display` marker is rejected, since `Display` is not derived for an arbitrary
type. Write an `impl Display` with a `fmt` body, or rely on the one a plain enum
or a newtype has.

### Format Traits

`${x:?}` / `${x:#?}` render through `Inspect`, which every type has, so they
need no bound.

`${x}` (`Display`) uses the type's `impl Display`. Primitives, `String`, plain enums (bare case name), and newtypes (inherited from the base type) have one. So do the prelude's sequences, tuples and ranges, where their elements allow it. Any other struct or variant needs a hand-written `impl Display`; otherwise `${x}` is a compile error and `${x:?}` gives its debug form. So `T: Display` certifies a real string representation. For example, `String::push_display` takes any `Display`. `${x:#}` is its [alternate form](./spec-literals.md#alternate-form).

```wado
fn describe<T>(v: &T) -> String { return `${v:?}`; }         // any type
fn label<T: Display>(v: &T) -> String { return `${v}`; }     // requires a `Display`
```

Rationale: [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

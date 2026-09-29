# Traits

A trait names methods that types implement. This chapter covers declaring,
implementing and bounding traits, how a call finds the impl that answers it,
where an impl may be written, and which impls the compiler derives. The traits
the prelude declares are in [Standard Traits](./spec-standard-traits.md).

## Declaring and Implementing Traits

Trait methods use static dispatch: every call is resolved at compile time.

A `trait` and an `interface` are declared in the type namespace: one name reaches one declaration wherever it is written. Neither denotes a type. Each names a set of operations, and no value has one as its type, so a type position naming one is a compile error. A trait reaches a type only as a bound (`fn f<T: Greet>(x: T)`).

<!-- {"fixture":"spec_traits_greet.wado"} -->

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
test {
    let p = Person { name: "Alice" };
    assert p.greet() == "Hello, Alice!";
}
```

### Supertraits

A trait can require its implementors to implement other traits. `impl Ord for T`
then fails unless `T` also implements `Eq`, and `T: Ord` alone is enough to use
`Eq`'s methods:

<!-- {"fixture":"spec_traits_supertraits.wado"} -->

```wado
trait Ord: Eq {
    fn cmp(&self, other: &Self) -> Ordering;
}

trait Circle: Shape + Display {
    fn radius(&self) -> i32;
}

// `T: Ord` implies `T: Eq`
fn dedup_sorted<T: Ord>(items: List<T>) -> List<T> {
    let mut out: List<T> = [];
    for let x of items {
        if out.is_empty() || out[out.len() - 1] != x { out.push(x); }
    }
    return out;
}

test {
    assert dedup_sorted([Id { n: 1 }, Id { n: 1 }, Id { n: 2 }]).len() == 2;
}
```

A trait that reaches itself through supertraits is an error. A method name
reachable through more than one of a receiver's bounds is ambiguous at the call
site; name the trait that declares it to resolve it (`Left::name(&x)`, a
[qualified call](#qualified-calls)). The bounds a body may name that way include
the implied ones, so `Eq::eq(&a, &b)` resolves under `T: Ord`.

### Multiple Traits

A struct can implement multiple traits:

<!-- {"fixture":"spec_traits_multiple.wado"} -->

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

test {
    let p = Person { name: "Alice", age: 30 };
    assert p.name() == "Alice" && p.age() == 30;
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

<!-- {"fixture":"spec_traits_inherent_wins.wado"} -->

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

test {
    let r = Robot { id: 1 };
    assert r.greet() == "Beep boop";  // inherent method wins
}
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
names the trait to import. Here the module imports `Speaker`, whose `shout`
comes from an impl of `Loud`, and not `Loud` itself:

<!-- {"fixture":"trait_error_unimported_trait_method.wado"} -->

```wado
use { Speaker } from "./sub/trait_scope_lib.wado";

export fn run() {
    let s = Speaker { id: 3 };
    // error: 'Loud' is not imported here: 'Speaker' has 'shout' through it;
    // import the trait to call it
    assert s.shout() == "speaker:3";
}
```

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

The second shape, where `Point` implements `Limit` and, as a struct, satisfies
`ReflectStruct`:

<!-- {"fixture":"error_ambiguous_value_blankets.wado"} -->

```wado
impl<T: Limit> Describe for T {
    fn describe(&self) -> String {
        return `limit:${T::limit()}`;
    }
}

impl<T: ReflectStruct> Describe for T {
    fn describe(&self) -> String {
        return `struct:${Reflect::<T>::type_name()}`;
    }
}

struct Point {
    x: i32,
}

impl Limit for Point {
    fn limit() -> i32 {
        return 7;
    }
}

export fn run() {
    let p = Point { x: 1 };
    // error: ambiguous blanket impls of 'Describe' for 'Point': 'T: Limit' and
    // 'T: ReflectStruct' apply, and nothing ranks them; write
    // 'impl Describe for Point'
    assert p.describe() == "limit:7";
}
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

A bound on a kind trait of the `Reflect` family holds only where the receiver's
members are visible ([Visibility](./spec-reflection.md#visibility)).

A blanket's receiver parameter is matched by position, not by spelling: a method
parameter named `T` inside the method is the method's own `T`.

#### Qualified Calls

Wado has no fully qualified `<Type as Trait>::method()` form, because a leading
`<` in expression position begins JSX. A call names its trait with the
trait-qualified form `Trait::method(recv, args…)` instead:

<!-- {"fixture":"spec_traits_qualified_calls.wado"} -->

```wado
fn show(p: P, f: &mut Formatter) {
    Display::fmt(&p, f);          // p implements two traits declaring `fmt`
}

fn name_of<T: Left + Right>(x: T) -> String {
    return Base::name(&x);        // supertrait diamond inside a generic body
}

test {
    let f = true;
    assert Take::<B>::take(&f, B { v: 1 }) == 10; // one trait's argument list, pinned
    let mut buf = "";
    show(P { v: 1 }, &mut Formatter::new(&mut buf));
    assert buf == "display" && name_of(P { v: 2 }) == "base";
}
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
from, so the trait-qualified form cannot name it. A reflection trait is the
exception: its one type argument is the subject
([Trait-Qualified Calls](./spec-reflection.md#trait-qualified-calls)).

Rationale: [WEP: Trait Resolution — One Order, Written Down](./wep-2026-09-01-trait-resolution.md).

### Default Method Implementations

Trait methods can have default implementations. Implementors can override them or use the defaults:

<!-- {"fixture":"spec_traits_default_methods.wado"} -->

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

test {
    assert Article { headline: "Rain" }.summary() == "Title: Rain";
    assert Report { headline: "Rain", body: "wet" }.summary() == "Rain: wet";
}
```

Default methods can call other trait methods (both required and default), and the calls are resolved against the implementing type.

### Associated Types

Traits can declare associated types: placeholder types that implementors specify:

<!-- {"fixture":"spec_traits_assoc_types.wado"} -->

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

test {
    let mut b = IntBox { value: 1 };
    b.set(42);
    assert b.get() == 42;
}
```

Within trait methods and implementations, `Self::TypeName` refers to the associated type. The type is resolved at compile time based on the implementing type.

### Bounded Associated Types

Associated types can have trait bounds that constrain what types can be used as the associated type:

<!-- {"fixture":"spec_traits_bounded_assoc.wado"} -->

```wado
trait Collection {
    type Element;
    type Builder: CollectionBuilder<Element = Self::Element, Output = Self>;
}

// A generic body relies on the bound: the builder builds a `C`
fn build_one<C: Collection>(b: C::Builder, e: C::Element) -> C {
    let mut b = b;
    b.add(e);
    return b.build();
}

test {
    assert build_one::<Bag>(BagBuilder { items: [] }, 7).items == [7];
}
```

Here `Builder` must implement `CollectionBuilder` with matching `Element` and `Output` types. The `Type = ConcreteType` syntax constrains associated types of the bound trait to specific types.

### Blanket Implementations

A blanket impl provides a trait implementation for all types that satisfy a given bound:

<!-- {"fixture":"spec_traits_blanket_impl.wado"} -->

```wado
// Any type that builds itself satisfies Collection automatically
impl<T: CollectionBuilder<Output = T>> Collection for T {
    type Element = T::Element;
    type Builder = T;
}

fn element<C: Collection>(e: C::Element) -> C::Element { return e; }

test {
    assert element::<Bag>(7) == 7;  // `Bag`'s `Element`, through the blanket
}
```

This avoids the need for explicit `impl Collection for ...` on every self-building type. `T::Element` names the `Element` that `T`'s `CollectionBuilder` impl binds.

### Impl Type Parameters Are Declared

An `impl` declares its type parameters in `impl<...>`, and that list is the only way to introduce one. A name in the target or the trait reference that the list does not hold is a type, and the module must declare it:

<!-- {"fixture":"spec_traits_impl_params.wado"} -->

```wado
impl<T> Stack<T> {                    // inherent
    fn size(&self) -> i32 { return self.items.len(); }
}
impl<T: Ord> Stack<T> {               // with a bound
    fn is_sorted(&self) -> bool { return self.items == self.items.sorted(); }
}
impl<K: Ord, V> Table<K, V> {         // every parameter listed
    fn len(&self) -> i32 { return self.keys.len(); }
}
impl<T> Default for Stack<T> {        // trait implementation
    fn default() -> Stack<T> { return Stack { items: [] }; }
}
impl Display for Stack<i32> {         // one instantiation declares none
    fn fmt(&self, f: &mut Formatter) { f.write_str(`${self.items.len()} ints`); }
}

test {
    let s = Stack { items: [1, 2] };
    assert s.size() == 2 && s.is_sorted() && `${s}` == "2 ints";
    assert Stack::<String>::default().size() == 0;
    assert Table { keys: [1], values: ["one"] }.len() == 1;
}
```

### Impl Type Parameters Must Be Determined

An `impl`'s target and trait reference between them must name every type parameter it declares. A use site determines them from the receiver and the trait arguments and from nothing else, so one neither mentions has no value to be given:

<!-- {"fixture":"spec_traits_impl_param_undetermined.wado"} -->

```wado
impl<A: Eq, T: Eq> Dup for T {  // ERROR: the type parameter `A` is not
                                //        constrained by the impl target
                                //        or the trait reference
    fn dup(&self) -> bool { return true; }
}
```

A bound determines one too, through the types it writes:

<!-- {"fixture":"spec_traits_impl_param_by_bound.wado"} -->

```wado
// `S` fixes `FieldTypes`, which fixes `..F`
impl<S: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect> Show for S {
    fn show(&self) -> String {
        let mut out = "";
        for let f of ReflectStruct::<S>::members() {
            out.push_str(`${f.get(self):?};`);
        }
        return out;
    }
}

test {
    assert Point { x: 1, y: 2 }.show() == "1;2;";
}
```

The bound's subject is not itself determined this way: `A: Eq` says what `A` must satisfy, not what `A` is.

### Trait Bounds

Type parameters can have trait bounds that constrain what types can be used:

<!-- {"fixture":"spec_traits_bounds.wado"} -->

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
impl<T: Ord> Pair<T> {
    pub fn sort(&mut self) { *self = self.sorted(); }
    pub fn sorted(&self) -> Pair<T> {
        return if self.first > self.second { Pair { first: self.second, second: self.first } } else { *self };
    }
}

// Bounded trait implementations - Pair<T> implements Eq only when T: Eq
impl<T: Eq> Eq for Pair<T> {
    fn eq(&self, other: &Self) -> bool {
        return self.first == other.first && self.second == other.second;
    }
}

test {
    assert max(3, 7) == 7;
    assert SortedPair { first: 1, second: 2 }.second == 2;
    assert PrintableOrd { value: 5 }.value.print() == "5";
    let mut p = Pair { first: 2, second: 1 };
    p.sort();
    assert p == Pair { first: 1, second: 2 };
}
```

### Bounds on Generic Traits

A bound that writes no argument names the declared default. `T: Add` is
`Add<Self>` (see [Arithmetic Operator Traits](./spec-standard-traits.md#arithmetic-operator-traits)),
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
parameter to write instead. A `struct` or `variant` declaration implements no
type, so a bound on its own parameter is the same error. The position in the
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

The unit of coherence is the package
([The Modules of a Package](./spec-packages.md#the-modules-of-a-package)). A
type or trait is local when a module of the current package declares it, and
foreign otherwise: a dependency's, and the standard library's (`core:*` and
`wasi:*`). A Wasm asset belongs to the package that imports it, but what it
declares is foreign.

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

<!-- {"fixture":"spec_traits_specific_wins.wado"} -->

```wado
impl<T> Tag for Box_<T> { fn tag(&self) -> String { return "general"; } }         // general
impl Tag for Box_<i32> { fn tag(&self) -> String { return "specific"; } }         // specific — wins for Box_<i32>

impl<..T> Tag for [..T] { fn tag(&self) -> String { return "general"; } }         // general
impl Tag for [i32, i32] { fn tag(&self) -> String { return "specific"; } }        // specific — wins for [i32, i32]

test {
    assert Box_ { v: 1 }.tag() == "specific" && Box_ { v: "s" }.tag() == "general";
    assert [1, 2].tag() == "specific" && [1, "s"].tag() == "general";
}
```

The specific impl applies to the instantiation it names; every other
instantiation takes the general one. Declaration order does not matter. This is
the generality rank of [The Order](#the-order), which also puts either of these
above a value blanket (`impl<T: Bound> Tag for T`).

This holds only for a trait impl, where the trait gives both methods one
signature. An inherent impl carries no such contract, so two inherent blocks
that reach a common receiver may not both define one method name:

<!-- {"fixture":"spec_traits_inherent_duplicate.wado"} -->

```wado
impl<T> Box_<T> { fn a(&self) -> String { return "any"; } }
impl Box_<i32> { fn a(&self) -> i32 { return self.v; } }   // ERROR: duplicate definition of `a`
```

Inherent blocks that reach no receiver in common may share a name.

Two impls that are general in the same way cannot be ordered at all, so a second
variadic impl of one trait is rejected where it is written:

<!-- {"fixture":"spec_traits_variadic_overlap.wado"} -->

```wado
impl<..T: Inspect> Tag for [..T] { fn tag(&self) -> String { return "inspect"; } }
impl<..T: Eq> Tag for [..T] { fn tag(&self) -> String { return "eq"; } }     // ERROR: overlapping variadic impls
```

Bounds do not separate them. A trait's own arguments do, since they make the
two impls of different traits:

<!-- {"fixture":"spec_traits_variadic_args.wado"} -->

```wado
impl<..T> Conv<i32> for [..T] { fn conv(&self, x: i32) -> String { return "i32"; } }          // OK
impl<..T> Conv<String> for [..T] { fn conv(&self, x: String) -> String { return "String"; } } // OK — a different trait

test {
    assert [1, true].conv(7 as i32) == "i32" && [1, true].conv("s") == "String";
}
```

### One Trait at Two Argument Lists

Wado has no function overloading. A module declares a free function name once
([Calls](./spec-functions.md#calls)), a type an inherent method name once
([Several Impl Blocks](./spec-functions.md#several-impl-blocks),
[Specific Impls Win](#specific-impls-win)), and a trait declaration a method
name once. Arity never tells two apart; default arguments cover optional
parameters.

The one overload set is one trait implemented for one type at several argument
lists. Each impl is legal, and the call's arguments choose between them:

<!-- {"fixture":"spec_traits_overload_select.wado"} -->

```wado
impl Take<A> for bool { fn take(&self, x: A) -> String { return "A"; } }
impl Take<B> for bool { fn take(&self, x: B) -> String { return "B"; } }

test {
    let f = true;
    let a: A = A { v: 1 };
    assert f.take(B { v: 1 }) == "B";   // OK: a named struct literal selects Take<B>
    assert f.take(a) == "A";            // OK: the local's declared type selects Take<A>
}
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

<!-- {"fixture":"spec_traits_overload_literal.wado"} -->

```wado
impl Take<i32> for bool { fn take(&self, x: i32) -> i32 { return 32; } }
impl Take<i64> for bool { fn take(&self, x: i64) -> i32 { return 64; } }

test {
    let f = true;
    f.take(42);                  // ERROR: the arguments do not select
}
```

A cast or the trait turbofish selects instead:

<!-- {"fixture":"spec_traits_overload_pinned.wado"} -->

```wado
test {
    let f = true;
    assert f.take(42 as i64) == 64;           // OK: the cast selects Take<i64>
    assert Take::<i64>::take(&f, 42) == 64;   // OK: the trait turbofish pins the list
}
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

<!-- {"fixture":"error_trait_argument_no_overload.wado"} -->

```wado
trait Take<T> with () {
    fn take(&self, v: &T) -> i32;
}

impl Take<i32> for bool {
    fn take(&self, v: &i32) -> i32 {
        return *v;
    }
}

impl Take<String> for bool {
    fn take(&self, v: &String) -> i32 {
        return v.len() as i32;
    }
}

export fn run() {
    let c = 'x';
    // error: no overload of 'take' accepts these arguments: the candidates are
    // 'Take<i32>' and 'Take<String>'
    assert true.take(&c) == 1;
}
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

<!-- {"fixture":"spec_traits_overload_static.wado"} -->

```wado
impl Enc<A> for M { fn make(v: A) -> i32 { return 1; } }
impl Enc<B> for M { fn make(v: B) -> i32 { return 2; } }

test {
    assert M::make(A { }) == 1;  // selects Enc<A>
    assert M::make(B { }) == 2;  // selects Enc<B>
}
```

`Type::from(x)` and `Type::try_from(x)` select this way among the type's `From`
and `TryFrom` impls, before `x` is typed. One admitted impl supplies the
argument's expected type, so `Wrapper::from(42)` beside `From<String>` and
`From<i64>` takes `From<i64>` and types `42` as `i64`. Several admitted impls
are ambiguous, and since `from` has no `self` the fix is a cast on the argument:

<!-- {"fixture":"spec_traits_from_literal.wado"} -->

```wado
impl From<i32> for Wrapper { fn from(v: i32) -> Wrapper { return Wrapper { v: v as i64 }; } }
impl From<i64> for Wrapper { fn from(v: i64) -> Wrapper { return Wrapper { v }; } }

test {
    let w = Wrapper::from(42);   // ERROR: a literal argument admits 'i32' and 'i64'
}
```

The cast selects:

<!-- {"fixture":"spec_traits_from_cast.wado"} -->

```wado
test {
    assert Wrapper::from(42 as i64).v == 42;    // OK
}
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

Two exceptions narrow the on-demand row. `Default` asks that every field carry a
default expression, not that every member satisfy `Default`
([Auto-Derivation](./spec-standard-traits.md#auto-derivation)). A `variant`
derives `Eq` but never `Ord`.

An on-demand impl is generated only where a use needs it, not for every declared
type. For `Eq` and `Ord` that use is an operator, a comparison method, or a
bound; for `Default`, a `T: Default` bound or a `T::default()` call; for serde,
a bound. A [marker](#compiler-synthesized-impl) needs one too.

A `fn`-typed member blocks `Eq`, `Ord`, and serde. A plain `enum` and a `flags`
type have no members, so they satisfy every structural obligation.
[Auto-derived Traits](./spec-types.md#auto-derived-traits) says what each
derived `Eq` and `Ord` compares.

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

<!-- {"fixture":"spec_traits_marker_impl.wado"} -->

```wado
use { Serialize, Deserialize } from "core:serde";

struct User {
    name: String,
    age: i32,
}

impl Serialize for User;      // compiler generates serialize method
impl Deserialize for User;    // compiler generates deserialize method

test {
    let json = to_string(&User { name: "Ann", age: 30 }).unwrap();
    assert json == `{"name":"Ann","age":30}`;
    assert from_string::<User>(json).unwrap().age == 30;
}
```

For `Eq`, `Ord`, `Default`, and serde, a marker is also a conformance check. A
type that is not eligible is a compile error at the marker's own span, with a
reason chain:

<!-- {"fixture":"spec_traits_marker_ineligible.wado"} -->

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

`${x:?}` and `${x:#?}` render through `Inspect`, which every type has, so they
need no bound.

`${x}` renders through the type's `impl Display`. The primitives and `String`
have one. So do the prelude's sequences, tuples and ranges, where their elements
allow it. A plain enum renders its bare case name, and a newtype inherits its
base's impl. Any other struct or variant needs a hand-written `impl Display`.
Without one, `${x}` is a compile error, and `${x:?}` gives the debug form.

So a `T: Display` bound certifies a real string representation, and
`String::push_display` takes any `Display`. `${x:#}` is the
[alternate form](./spec-literals.md#alternate-form).

<!-- {"fixture":"spec_traits_format_bounds.wado"} -->

```wado
fn describe<T>(v: &T) -> String { return `${v:?}`; }         // any type
fn label<T: Display>(v: &T) -> String { return `${v}`; }     // requires a `Display`

test {
    assert describe(&Point { x: 1, y: 2 }) == "Point { x: 1, y: 2 }";
    assert label(&42) == "42";
}
```

Rationale: [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

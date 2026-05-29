# Research: Variadic Generics / Variadic Templates

Survey of variadic type parameter designs across languages, as background for a potential
Wado WEP on variadic generics for tuple support.

## Motivation

Wado's tuple type `[T, U, V]` is heterogeneous and fixed-arity. Writing trait
implementations or generic functions that work over tuples of **any** length requires
either generating per-arity code or accepting a single opaque `T` that is instantiated to
any tuple type (as in WEP-2026-02-10). Neither approach scales cleanly to expressing
constraints like "a tuple where every element implements `Eq`."

The primary use cases driving this research:

1. **Arbitrary-length tuple trait impls**: `Eq`, `Ord`, `Default`, `Serialize` for any
   tuple without per-arity boilerplate.
2. **Tuple transformation functions**: `concat`, `prepend`, `zip` with precise return types.
3. **Constraint propagation**: expressing that "all elements of this tuple satisfy trait T."

---

## C++ — Variadic Templates (C++11+)

### Status

Implemented and mature. Extended with fold expressions in C++17, pack indexing in C++26.

### Syntax

```cpp
// Type parameter pack: typename... Args
template<typename... Ts>
struct Tuple {};                        // empty specialization for zero-element pack

// Function parameter pack
template<typename... Args>
void f(Args... args);

// Pack expansion: pattern...
template<typename... Ts>
void g(Ts... args) {
    f(&args...);      // expands to f(&a0, &a1, &a2, ...)
}

// sizeof...(T) — compile-time count
template<typename... Ts>
constexpr size_t count = sizeof...(Ts);

// Fold expressions (C++17)
template<typename... Args>
auto sum(Args... args) {
    return (args + ... + 0);    // left fold with initial value
}

// Pack indexing (C++26)
template<typename... Args>
auto first(Args... args) {
    return args...[0];
}
```

### Expansion contexts

Packs can be expanded in: function arguments, template arguments, brace-enclosed
initializers, base class lists, member initializer lists, lambda captures.

### Tuple use case — `std::tuple`

`std::tuple<Ts...>` is implemented using recursive template specialization over the pack.
Element access uses `std::get<N>` (not subscript), since the type of each element differs.

```cpp
// Mapping over a tuple (C++17 std::apply + lambda)
template<typename... Ts, typename F>
auto map_tuple(std::tuple<Ts...> t, F f) {
    return std::apply([&](auto&&... xs) {
        return std::make_tuple(f(xs)...);
    }, t);
}
```

`std::apply` unpacks the tuple into individual function arguments, then `f(xs)...`
applies `f` to each and repacks.

### Limitations

- Error messages are notoriously cryptic when expansion goes wrong.
- Pre-C++17, recursive template specialization was required for iteration — verbose and slow
  to compile.
- No direct "for each element" construct; fold expressions partially address this.
- Each unique instantiation generates separate code (monomorphization / binary bloat).
- Multiple packs in the same expansion must have equal lengths (no compiler-enforced
  constraint; mismatch is a hard error).

---

## TypeScript — Variadic Tuple Types (TS 4.0+)

### Status

Implemented since TypeScript 4.0 (August 2020).

### Syntax

```typescript
// Generic rest element in tuple type
type Concat<T extends unknown[], U extends unknown[]> = [...T, ...U];

// Rest element at any position (not just the end)
type Prefix<T extends unknown[]> = [string, ...T];
type Suffix<T extends unknown[]> = [...T, number];
type Middle<T extends unknown[], U extends unknown[]> = [string, ...T, number, ...U];

// The extends unknown[] constraint says "T must be an array or tuple type"
// (tuple types are a subtype of unknown[])
```

### Key behaviors

**Spreading into a tuple type** replaces the rest element with the argument's elements:

```typescript
type T1 = Prefix<[boolean, null]>;           // [string, boolean, null]
type T2 = Concat<[number, string], [boolean]>; // [number, string, boolean]
```

**Inference** works in function parameter position:

```typescript
function concat<T extends unknown[], U extends unknown[]>(
    a: [...T],
    b: [...U]
): [...T, ...U] {
    return [...a, ...b];
}

const result = concat([1, "hello"], [true, 42]);
// result: [number, string, boolean, number]
```

**Mapping over a tuple type** uses mapped types:

```typescript
type MapToString<T extends unknown[]> = {
    [K in keyof T]: string;
};
// MapToString<[number, boolean, null]> = [string, string, string]

type Promisify<T extends unknown[]> = {
    [K in keyof T]: Promise<T[K]>;
};
// Promisify<[number, string]> = [Promise<number>, Promise<string>]
```

**Normalization** after instantiation: optional elements before required ones become
required; a single rest element reduces to an array type.

### Limitations

- No runtime reflection on type structure (types are erased at runtime).
- Cannot "loop" over tuple elements at the type level; must use recursive conditional types,
  which are verbose and can hit recursion depth limits.
- Inference on deeply nested variadic patterns can be slow.
- Labeled tuple elements cannot contain a rest spread in all contexts.

### Key use case — promisify

```typescript
type InferCallbackArgs<T> =
    T extends (...t: [...infer Args, (res: infer R) => void]) => void
        ? [Args, R]
        : never;
```

---

## Swift — Parameter Packs (SE-0393/SE-0398/SE-0399, Swift 5.9+)

### Status

Implemented in Swift 5.9 (June 2023). Three proposals:

- SE-0393: value and type parameter packs (core feature)
- SE-0398: variadic types
- SE-0399: tuple-related fixes

### Syntax

```swift
// Declare a type parameter pack with `each`
func process<each T>() { }

// Pack expansion type: `repeat each T`
// — the pack is "spread" by writing `repeat pattern(each T)`
func apply<each T>(_ values: repeat each T) { }

// Constraint on the entire pack
func constrained<each T: Equatable>(_ values: repeat each T) -> Bool { }

// Return type built from the pack
func identity<each T>(_ values: repeat each T) -> (repeat each T) {
    return (repeat each values)
}
```

### Multi-pack: shape must match

When two or more packs appear in the same `repeat` expression, they must have equal length
at every call site (enforced by the compiler):

```swift
func zip<each T, each U>(
    _ first: repeat each T,
    _ second: repeat each U
) -> (repeat (each T, each U)) {
    return (repeat (each first, each second))
}

zip(1, "hello", 3.14 as Float, true)   // T = {Int, String, Float, Bool}
//  ^--- ERROR: T and U must have same shape
```

### Disambiguating multiple packs with labels

When two packs appear as parameters, labels are required to mark the boundary:

```swift
// ERROR: ambiguous where the first pack ends and the second begins
// func bad<each A, each B>(_ a: repeat each A, _ b: repeat each B) {}

// OK: labels delimit each pack
func ok<each A, each B>(first a: repeat each A, second b: repeat each B) {}
```

### Key motivating use case — SwiftUI TupleView

Before parameter packs, SwiftUI needed 10+ separate overloads:

```swift
// Old: TupleView<T0, T1>, TupleView<T0, T1, T2>, ... up to T9
// New: single definition
public struct TupleView<each Content: View>: View {
    let content: (repeat each Content)
}
```

### Pack iteration in the body

`repeat` in expression position expands a pattern for each element:

```swift
func allEqual<each T: Equatable>(_ lhs: repeat each T, _ rhs: repeat each T) -> Bool {
    var result = true
    repeat (result = result && each lhs == each rhs)
    return result
}
```

### Limitations

- Labeled tuple elements cannot contain pack expansion types in all contexts.
- No pack indexing (accessing the Nth element by constant index is not yet supported).
- Multi-pack `repeat` requires equal shape; no "apply packs of different lengths" construct.
- Verbose compared to C++'s `...`.

---

## D Language — Variadic Template Sequences

### Status

Implemented and mature. Built-in language feature.

### Syntax

```d
// `T...` declares a template sequence parameter
void process(Args...)(Args args) { }

// Length and indexing: sequence behaves like a compile-time array
void check(T...)(T args) {
    static assert(T.length >= 1);    // compile-time length
    auto first = args[0];            // first element (type T[0])
    auto rest  = args[1 .. $];       // slice (like array slice)
}

// foreach over a sequence (compile-time unrolling)
void printAll(Args...)(Args args) {
    foreach (t; args) {
        import std.stdio : writeln;
        writeln(t);
    }
}

// static if for compile-time branching over sequences
void recurse(Args...)(Args args) {
    static if (Args.length == 0) {
        // base case
    } else {
        process(args[0]);
        recurse(args[1 .. $]);
    }
}
```

### Type-level sequence mapping

```d
// Map a template alias over a sequence to produce a new sequence
template Map(alias F, T...) {
    static if (T.length == 0)
        alias Map = AliasSeq!();
    else
        alias Map = AliasSeq!(F!(T[0]), Map!(F, T[1 .. $]));
}

template Pointer(T) { alias Pointer = T*; }
alias PtrTypes = Map!(Pointer, int, string, double);
// PtrTypes = (int*, string*, double*)
```

### Key strengths vs. C++

- Sequences index directly like arrays (`args[0]`, `args[1..$]`) — cleaner than C++'s
  recursive template expansion.
- `static if` is a clean compile-time conditional (no need for SFINAE or `if constexpr`).
- `foreach` over a sequence unrolls at compile time — closest to Wado's existing
  compile-time `for let v of tuple`.

### Limitations

- Each unique instantiation still generates separate code (binary bloat).
- Recursive patterns for higher-order operations (map, filter) still require boilerplate.

---

## Dart — Records (Dart 3.0+) + No Variadic Generics

### Status

Dart 3.0 (May 2023) added **Records** as a native fixed-size heterogeneous tuple. Variadic
generics are **not implemented** — open proposals exist but have no timeline.

### Records: fixed-size heterogeneous tuples

```dart
// Positional record
(int, String, bool) record = (42, "hello", true);
int n    = record.$1;   // field access via $1, $2, $3 ...
String s = record.$2;

// Named fields
({int id, String name}) person = (id: 1, name: "Alice");
int id     = person.id;
String name = person.name;

// Mixed
(double, name: String) mixed = (3.14, name: "pi");

// Pattern destructuring
final (x, y) = (10, 20);
final (:id, :name) = person;
```

Records are **structurally typed**: two record types from different libraries with identical
field shapes are the same type. This is unlike most languages where records/tuples are
nominal.

### Generic functions over records (limited)

A function can be generic in the _element types_ of a fixed-shape record:

```dart
// Works: generic in one field's type
(T, T) getPair<T>(T a, T b) => (a, b);

(int, int)       ints    = getPair(1, 2);
(String, String) strings = getPair("a", "b");
```

But the _shape_ (arity) cannot be generic:

```dart
// NOT POSSIBLE in Dart today:
// class Tuple<...T> { ... }   // no variadic type parameters
// (T, T, T) getTriple<T, ...> // cannot vary arity generically
```

### Absence of variadic generics: practical impact

Without variadic generics, the Dart ecosystem must work around the limitation:

```dart
// Must write separate classes for each arity (from package:tuple):
class Tuple2<T1, T2>  { T1 item1; T2 item2; }
class Tuple3<T1, T2, T3> { ... }
// ... up to Tuple7

// Provider library:
Consumer<A>
Consumer2<A, B>
Consumer3<A, B, C>
// ... Consumer6

// Cannot write a type-safe generic zip:
// List<(T, U)> zip<T, U>(List<T> a, List<U> b) — works for two lists
// But no generic "zip N lists of different types" without codegen
```

### Proposals for Dart variadic generics

Two syntax proposals have been discussed in the Dart language repository (issues #1774 and
#2532), neither implemented:

```dart
// Proposal A: spread syntax
class Tuple<...T> { }
Tuple<int, String, bool> t = ...;

// Proposal B: array-of-types syntax (favored)
class Tuple<T[]> { }
Tuple<(int, String, bool)> t = ...;
```

### Key comparison: Dart Records vs. TypeScript/Swift

| Feature                    | Dart Records | TypeScript tuples     | Swift tuples          |
| -------------------------- | ------------ | --------------------- | --------------------- |
| Fixed-size heterogeneous   | Yes          | Yes                   | Yes                   |
| Structural typing          | Yes          | No (nominal)          | No (nominal)          |
| Generic over shape (arity) | **No**       | Yes (variadic tuples) | Yes (parameter packs) |
| Pattern matching           | Yes          | Limited               | Yes                   |

### Relevance to Wado

Dart demonstrates that records alone (without variadic generics) cannot express generic
tuple operations. The structural typing is an interesting design choice (Wado uses `[T, U]`
notation which is also structural), but the lack of variadic parameters means Dart users
face the same per-arity boilerplate problem that motivates this WEP research.

---

## Rust — Variadic Generics (Proposed, Not Yet Implemented)

### Status

Long-standing open design problem. Tracking issue open since ~2013 (RFC #376). As of
2025, still not implemented in stable Rust.

### Proposed syntax (RFC #376 draft)

```rust
// `..T` as a variadic type pack
fn process<..T>(args: (..T)) { }

// Pack expansion: `..args` unpacks in value position
fn forward<..T>(args: (..T)) {
    some_fn(..args);    // expands to positional arguments
}

// Return type built from pack
fn identity<..T>(args: (..T)) -> (..T) { args }
```

### Key challenges

1. **Trait solver coherence**: variadic impls (e.g., `impl<..T: Eq> Eq for (..T)`) must not
   break coherence rules. Rust's solver currently resolves traits in a fixed order; variadic
   packs require the solver to handle open-ended type sequences.

2. **Lifetime interactions**: `'static` and other lifetime bounds on pack elements are
   unsolved. Rust's borrow checker needs to reason about lifetime relationships across all
   pack elements simultaneously.

3. **Inference complexity**: determining where one pack ends and another begins when two packs
   appear in the same signature requires greedy assignment heuristics that may produce
   surprising results.

4. **Error quality**: errors that arise from failed pack constraints must point to the specific
   failing element, which is much harder than for scalar type errors.

5. **Compile-time explosion**: each unique instantiation generates code; large packs or many
   distinct instantiations can cause extreme compile times and binary bloat.

### Why Rust is stuck

The 2025 blog post "A madman's guide to variadic generics" (internals.rust-lang.org)
identifies several non-viable approaches and concludes that the main blocker is: any design
that allows bounds to be checked on individual pack elements interacts deeply with the trait
solver's coherence model, which was not built with variadic sequences in mind.

Wado does not have Rust's borrow checker or lifetime system, and its trait solver is simpler.
The specific obstacles Rust faces do not directly apply.

### Current workarounds in Rust

- **Procedural macros**: generate per-arity impls at build time (e.g., for tuples up to
  arity 12). Used in the standard library for `impl<T: Eq, U: Eq> Eq for (T, U)` etc.
- **HList libraries**: type-level linked list approach (same as Haskell), with heavy
  boilerplate.
- **Const generics**: array-like variadics (all elements same type, length as const param).

---

## Summary Comparison

| Language   | Status          | Syntax                      | Tuple support              | Iteration              | Multi-pack                   |
| ---------- | --------------- | --------------------------- | -------------------------- | ---------------------- | ---------------------------- |
| C++        | Implemented     | `typename... Ts`, `args...` | `std::tuple<Ts...>`        | Fold expressions       | Equal length                 |
| TypeScript | Implemented     | `[...T, ...U]` in types     | First-class                | Mapped types           | Spread at call               |
| Swift      | Implemented     | `each T`, `repeat each T`   | `(repeat each T)`          | `repeat` expression    | Equal shape, labels required |
| D          | Implemented     | `T...` sequences            | Via AliasSeq               | `foreach`, `static if` | Equal length                 |
| Dart       | Not implemented | Proposed (`T[]` or `...T`)  | Records (fixed-arity only) | N/A                    | N/A                          |
| Rust       | Not implemented | `..T` (proposed)            | Proposed                   | Not yet specified      | Not yet specified            |

### Key observations for Wado

1. **All implemented languages use monomorphization**: each concrete pack instantiation
   generates distinct code. Runtime is zero-cost; the cost is compile time and binary size.

2. **The fundamental iteration primitives differ**:
   - C++: fold expressions (`args op ...`), pack expansion in expressions
   - Swift: `repeat pattern(each pack)` in expression position
   - D: `foreach (t; args)` — closest to Wado's `for let v of tuple`
   - TypeScript: mapped types at the type level; no value-level iteration

3. **Multi-pack requires shape discipline**: when two packs appear in one context, all
   languages require them to be the same length (C++, D: implicit; Swift: explicit label
   disambiguation).

4. **TypeScript's type-level tuple mapping** (`{ [K in keyof T]: Promise<T[K]> }`) is
   powerful and relevant to the Wado use case of expressing "all elements of this tuple
   satisfy trait X."

5. **D's `foreach` over sequences** is semantically the same as Wado's existing compile-time
   `for let v of tuple` — both unroll into per-element blocks at compile time. D achieves
   this without a separate enumeration mechanism.

6. **Swift's label requirement** for multiple packs is a design lesson: if Wado allows two
   packs in one function signature, some disambiguation mechanism is needed.

7. **Dart demonstrates the limit of records without variadic generics**: structural typing
   of fixed-arity tuples is elegant, but without variadic parameters you still need per-arity
   boilerplate (`Consumer2`, `Consumer3`, ...). Records alone do not solve the problem.

8. **Rust's struggles are specific to its trait solver and borrow checker** — neither of
   which Wado has. Wado's simpler trait system should make variadic trait impls more
   straightforward to design and implement.

---

## Open Questions for a Wado WEP

1. **Syntax choice** for pack declaration: `..T` (consistent with Wado's existing rest
   pattern `..`), `...T` (TypeScript-style), or `each T` (Swift-style, clearest intent)?

2. **Pack in tuple type**: `[..T]` vs `[T...]` vs `[repeat T]`?

3. **Value-level pack construction**: how to express "build a new tuple by calling
   `T::default()` for each type T in the pack"? Options:
   - `[..T::default()]` — pack expansion in tuple literal
   - `[repeat T::default()]` — new `repeat` keyword
   - Compiler-synthesized only (no user-written expression)

4. **Multi-pack**: phase it out of the initial design (YAGNI), or include from day one?
   The concat/zip use cases are strong motivators. Swift's label approach vs. C++'s implicit
   equal-length requirement.

5. **Definition-time vs. monomorphization-time type checking**: WEP-2026-02-10 already
   chose the C++ template model (monomorphization-time). Variadic generics should follow the
   same model, or is there a case for partial definition-time checking (TypeScript-style)?

6. **Interaction with `for let v of tuple`**: the existing compile-time unrolling already
   handles consuming a tuple whose type is `[..T]` once T is instantiated. Does it work
   without changes, or does the elaborator need to recognize that `[..T]` expands during
   monomorphization?

---

## References

- C++: [cppreference — Parameter Pack](https://en.cppreference.com/w/cpp/language/parameter_pack)
- C++: [C++20 Idioms for Parameter Packs — Stanford](https://www.scs.stanford.edu/~dm/blog/param-pack.html)
- C++: [From Variadic Templates to Fold Expressions — ModernCpp](https://www.modernescpp.com/index.php/from-variadic-templates-to-fold-expressions/)
- TypeScript: [TS 4.0 Release Notes — Variadic Tuple Types](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-4-0.html)
- TypeScript: [Computing with Tuple Types — 2ality](https://2ality.com/2025/01/typescript-tuples.html)
- Swift: [SE-0393 — Value and Type Parameter Packs](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0393-parameter-packs.md)
- Swift: [SE-0398 — Variadic Types](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0398-variadic-types.md)
- Swift: [Hacking with Swift — Variadic Generics](https://www.hackingwithswift.com/swift/5.9/variadic-generics)
- D: [D Templates — dlang.org](https://dlang.org/spec/template.html)
- D: [Compile-time Sequences in D](https://dlang.org/articles/ctarguments.html)
- Dart: [Dart Records Documentation](https://dart.dev/language/records)
- Dart: [Records Feature Specification (GitHub)](https://github.com/dart-lang/language/blob/main/accepted/3.0/records/feature-specification.md)
- Dart: [Variadic Generics Proposal — Issue #1774](https://github.com/dart-lang/language/issues/1774)
- Dart: [Variadic Generics Proposal — Issue #2532](https://github.com/dart-lang/language/issues/2532)
- Rust: [Draft RFC: variadic generics — RFC #376](https://github.com/rust-lang/rfcs/issues/376)
- Rust: [A madman's guide to variadic generics — Rust Internals](https://internals.rust-lang.org/t/a-madmans-guide-to-variadic-generics/19605)
- Rust: [Variadic Generics ideas that won't work for Rust — PoignardAzur](https://poignardazur.github.io/2025/07/09/variadic-generics-dead-ends/)

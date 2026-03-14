# WEP: Variadic Type Parameters

## Context

Wado's tuple type `[T, U, V]` is a fixed-arity product type. Generic functions and trait
implementations that work over tuples of **any** length currently require either:

- Writing separate implementations for each arity (boilerplate, capped at some maximum), or
- Accepting a single generic `T` that is instantiated to any tuple type (the current approach
  in [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md))

The second approach works for functions that consume tuple elements uniformly
(e.g., `for let v of values`), but it cannot express:

- Trait implementations for tuples of any length (`impl<..T: Eq> Eq for [..T]`)
- Functions whose return type depends on the input tuple's structure (`fn concat`)
- Type-level constraints on each element (`..T: Trait` vs. a single opaque `T`)

### Motivating Examples

**Trait impl for any-length tuple** — without variadic parameters, the standard library
would need to generate a fixed set of per-arity impls and silently fail for larger tuples:

```wado
// Must implement separately for each arity (unwieldy and capped):
impl Eq for [] { ... }
impl<T: Eq> Eq for [T] { ... }
impl<T: Eq, U: Eq> Eq for [T, U] { ... }
impl<T: Eq, U: Eq, V: Eq> Eq for [T, U, V] { ... }
// ... manually up to some limit

// With variadic type parameters, one impl covers all arities:
impl<..T: Eq> Eq for [..T] { ... }
```

**Tuple transformation** — a function that prepends a value to a tuple of any length:

```wado
// Without variadic: cannot express the return type generically
fn prepend<..T>(head: i32, tail: [..T]) -> [i32, ..T] { ... }
```

**Tuple concatenation** — joining two tuples whose element types are known at compile time:

```wado
fn concat<..T, ..U>(a: [..T], b: [..U]) -> [..T, ..U] {
    return [..a, ..b];
}
```

### Relationship to Compile-Time Tuple Enumeration

[WEP: Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)
already allows iterating over tuple elements at compile time:

```wado
fn process<Values>(values: Values) {
    for let v of values { ... }  // unrolled when Values is concrete
}
```

Variadic type parameters complement this: they add **type-level** expressiveness (describing
what the tuple contains and constraining its elements), while the compile-time `for let v of`
provides the **value-level** iteration primitive.

### Language Survey

#### C++ (variadic templates, since C++11)

C++ uses a trailing `...` in template parameter lists to declare parameter packs:

```cpp
template<typename... Ts>           // type pack
struct Tuple {};

template<typename... Ts>
void process(Ts... args) {         // value pack
    (handle(args), ...);           // fold expression (C++17): call handle() for each arg
}

template<typename T, typename... Ts>
auto prepend(T head, std::tuple<Ts...> tail) -> std::tuple<T, Ts...> {
    return std::apply([&](auto... ts) {
        return std::make_tuple(head, ts...);
    }, tail);
}
```

Key features: `sizeof...(Ts)` counts pack elements; C++17 fold expressions reduce over
packs; C++26 adds pack indexing (`Ts...[0]`). Tuple transformation uses `std::apply` to
unpack into a function call, then re-pack the results with `...`. Error messages are
notoriously poor.

#### TypeScript (variadic tuple types, since 4.0)

TypeScript allows spreading generic type parameters in tuple type positions:

```typescript
type Concat<T extends unknown[], U extends unknown[]> = [...T, ...U];

function concat<T extends unknown[], U extends unknown[]>(
    a: [...T], b: [...U]
): [...T, ...U] {
    return [...a, ...b];
}

// Rest elements can appear anywhere in a tuple type:
type Middle<T extends unknown[]> = [string, ...T, number];
```

Inference is strong: the compiler can infer `T` and `U` from argument types and propagate
the combined type to the return. The constraint `T extends unknown[]` distinguishes
array-spread (homogeneous) from tuple-spread (heterogeneous).

#### Swift (parameter packs, SE-0393, since Swift 5.9)

Swift uses `each T` to declare a type parameter pack and `repeat each T` to expand it:

```swift
func zip<each T, each U>(
    _ first: repeat each T,
    _ second: repeat each U
) -> (repeat (each T, each U)) {
    return (repeat (each first, each second))
}
```

The `repeat` keyword expands a pattern for each element in the pack. Multiple packs in one
`repeat` expression must have equal length (shape-checked at the call site). Swift uses this
to eliminate TupleView's 10-view limit in SwiftUI.

#### Rust (not yet implemented)

Rust has no stable variadic generics. The long-standing tracking issue
([RFC #376](https://github.com/rust-lang/rfcs/issues/376)) remains open. Key challenges:

- Integration with the borrow checker and lifetime system
- Interaction with trait solver coherence rules
- Compile-time complexity from monomorphization of large instantiation trees
- Error message quality

Community workarounds use procedural macros (generating per-arity impls) or `HList`
libraries (type-level linked lists). The 2025 blog post "A madman's guide to variadic
generics" identifies several non-viable approaches and highlights open design questions.

#### Summary

| Language   | Feature                      | Status                | Key mechanism              |
| ---------- | ---------------------------- | --------------------- | -------------------------- |
| C++        | Variadic templates           | Implemented (C++11)   | `typename... Ts`, `args...` |
| TypeScript | Variadic tuple types         | Implemented (TS 4.0)  | `[...T, ...U]` in types    |
| Swift      | Parameter packs (SE-0393)    | Implemented (5.9)     | `each T`, `repeat each T`  |
| Haskell    | `HList` library              | Library (not built-in) | GADTs + DataKinds          |
| Rust       | Variadic generics            | Proposed, unimplemented | —                       |

### Design Goals for Wado

1. **Consistency with existing syntax**: `..` is already used for rest patterns in tuple
   and struct destructuring. Reuse it for packs.
2. **Orthogonal to compile-time tuple enumeration**: variadic parameters describe the type;
   `for let v of tuple` handles value-level iteration.
3. **Minimal syntax additions**: avoid new keywords where existing constructs suffice.
4. **YAGNI**: phase the design; start with the single-pack case, which covers the most
   important use cases.

---

## Decision

### 1. Type Pack Parameters

A **type pack** is declared in a generic parameter list using the `..` prefix:

```wado
fn foo<..T>() { }                // one unconstrained type pack
fn foo<..T: Trait>() { }         // all element types must implement Trait
fn foo<..T: Trait + Other>() { } // multiple bounds
fn foo<A, ..T, B>() { }          // mix of scalar and pack parameters
```

Type packs are distinct from scalar type parameters:

| Declaration | Represents                                  |
| ----------- | ------------------------------------------- |
| `T`         | Exactly one type                            |
| `..T`       | A sequence of zero or more types            |

A bound `..T: Trait` requires every element type in the pack to implement `Trait`.

### 2. Pack in Tuple Types

A type pack can appear in a tuple type using `..T` notation:

```wado
[..T]           // tuple whose elements are exactly T's types
[i32, ..T]      // i32 followed by T's types
[..T, i32]      // T's types followed by i32
[..T, ..U]      // T's types followed by U's types (requires two packs)
```

Concrete examples (with `T` instantiated to `[bool, String]`):

| Type expression | Instantiated result |
| --------------- | ------------------- |
| `[..T]`         | `[bool, String]`    |
| `[i32, ..T]`    | `[i32, bool, String]` |
| `[..T, i32]`    | `[bool, String, i32]` |

### 3. Generic Functions over Packs

Functions declare type packs in their generic parameter list and use `[..T]` in signatures:

```wado
// Consume all elements (use compile-time tuple enumeration in body)
fn debug_all<..T: Debug>(values: [..T]) -> String {
    let mut parts: Array<String> = [];
    for let v of values {
        parts.append(`{v:?}`);
    }
    return parts.join(", ");
}

// Transform a tuple: append an element
fn push<..T>(tuple: [..T], last: i32) -> [..T, i32] {
    return [..tuple, last];
}

// Two packs: concatenate (see §5 for value-level spread)
fn concat<..T, ..U>(a: [..T], b: [..U]) -> [..T, ..U] {
    return [..a, ..b];
}
```

The body uses existing constructs: `for let v of tuple` for uniform consumption,
value-level spread `[..a, ..b]` for construction (§5).

### 4. Trait Implementations for Arbitrary-Length Tuples

The most important use case: one `impl` covers tuples of any length:

```wado
impl<..T: Eq> Eq for [..T] {
    fn eq(&self, other: &Self) -> bool {
        for let [i, v] of self.enumerate() {
            if v != other[i] { return false; }
        }
        return true;
    }
}

impl<..T: Ord> Ord for [..T] {
    fn cmp(&self, other: &Self) -> Ordering {
        for let [i, v] of self.enumerate() {
            let c = v.cmp(&other[i]);
            if c matches { Equal } { } else { return c; }
        }
        return Ordering::Equal;
    }
}
```

The body uses compile-time tuple enumeration from WEP-2026-02-10: `for let [i, v] of self.enumerate()`
unrolls into one block per element type at monomorphization, and each block is type-checked
with `v` having the specific element type `T_i`.

The bound `..T: Eq` ensures that `v != other[i]` is valid for every element, regardless of
which concrete types fill the pack at a given call site.

### 5. Value-Level Pack Spread

**Tuple spread** allows expanding a tuple value into a new tuple literal using `..`:

```wado
// [..a] spreads all elements of tuple a into this tuple literal
let a: [i32, bool] = [1, true];
let b: [i32, bool, String] = [..a, "hello"];  // [1, true, "hello"]

// Two spreads (concat)
fn concat<..T, ..U>(a: [..T], b: [..U]) -> [..T, ..U] {
    return [..a, ..b];
}
```

Tuple spread is valid in tuple literals and in `return` expressions of type `[..T, ...]`. The
compiler resolves the result type from the spread operands' types, verified at
monomorphization.

This is distinct from the existing rest pattern `..` in destructuring (which _ignores_
elements). In a tuple literal context, `..a` means _include_ all elements of `a`.

### 6. Type-Level Pack Expansion (for Construction)

Sometimes an implementation needs to **construct** a tuple by calling a static method once
for each type in a pack. For example, `Default` for tuples:

```wado
impl<..T: Default> Default for [..T] {
    fn default() -> [..T] {
        return [..T::default()];
    }
}
```

`[..T::default()]` is a **pack construction expression**: the compiler expands it by calling
`T_i::default()` for each type `T_i` in pack `T` and assembling the results into a tuple.

For `T = [i32, String]`, this expands to `[i32::default(), String::default()]` = `[0, ""]`.

This differs from value-level spread `[..a]` (where `a` is a tuple value) in that `T` is a
type pack name, not a value. The notation `T::method()` in this context denotes a
_per-element-type_ static method call, executed once per type in the pack.

Pack construction expressions are only valid inside variadic generic contexts (where `T` is
declared as `..T`) and only in tuple literal position.

### 7. Multi-Pack Shape Constraint

When a function declares two or more type packs, call sites must supply packs of equal length
(**same shape**):

```wado
fn zip_with<..T, ..U>(a: [..T], b: [..U], ...) -> ... { }

zip_with([1, true], ["x", "y"], ...);  // OK: T = [i32, bool], U = [String, String]
zip_with([1, true], ["x"], ...);       // ERROR: T has 2 elements, U has 1
```

Shape mismatch is a compile-time error. Single-pack functions have no such constraint.

### 8. Type-Level Pack Length (`..len`)

Inside a variadic generic context, the number of elements in a pack is accessible as a
compile-time constant:

```wado
fn arity<..T>(_: [..T]) -> i32 {
    return ..len(T);  // compile-time constant
}
```

This is modeled on C++'s `sizeof...(T)`. It enables length-conditional behavior without
depending on runtime values.

### 9. Monomorphization Semantics

Variadic type parameters follow the same **monomorphization-at-call-site** model used for
all other generic parameters in Wado. Each unique instantiation of a pack generates a
separate copy of the function or impl.

For `fn debug_all<..T: Debug>(values: [..T])`:

| Call                              | Monomorphized variant                        |
| --------------------------------- | -------------------------------------------- |
| `debug_all([42, true])`           | `debug_all__i32_bool([42, true])`            |
| `debug_all([1.5, "x", 'z'])`      | `debug_all__f64_String_char([1.5, "x", 'z'])` |
| `debug_all([])`                   | `debug_all__empty([])`                       |

Type checking of the body (including `for let v of values`) occurs after substitution, as
specified by WEP-2026-02-10's C++ template model.

### 10. Interaction with Existing Features

#### Compile-Time Tuple Enumeration (WEP-2026-02-10)

`for let v of tuple` already unrolls at monomorphization for concrete tuple types.
With variadic parameters, the same mechanism applies when the tuple's type is `[..T]`:
after `T` is instantiated to a concrete pack, `for let v of tuple` unrolls as before.
No new compiler mechanism is needed for iteration.

#### Trait Bounds (WEP-2026-02-07)

Bounds `..T: Trait` are enforced the same way as scalar bounds: at each call site, the
compiler verifies that every type in the pack satisfies `Trait`. An unsatisfied bound
produces a clear error pointing to the call site and the offending element type.

#### Tuple Destructuring (WEP-2026-02-22)

Tuple destructuring patterns (`let [a, b, ..] = tuple`) are unaffected. The `..` rest
pattern in a _pattern_ context remains distinct from `..T` in a _type_ or _expression_ context.
The compiler distinguishes them by position: `..T` is only valid as a generic parameter
declaration or in type/expression contexts.

---

## Examples

### Standard Library: `Eq`, `Ord`, `Default` for Tuples

```wado
// Eq
impl<..T: Eq> Eq for [..T] {
    fn eq(&self, other: &Self) -> bool {
        for let [i, v] of self.enumerate() {
            if v != other[i] { return false; }
        }
        return true;
    }
}

// Ord (lexicographic)
impl<..T: Ord> Ord for [..T] {
    fn cmp(&self, other: &Self) -> Ordering {
        for let [i, v] of self.enumerate() {
            let c = v.cmp(&other[i]);
            if c matches { Equal } { } else { return c; }
        }
        return Ordering::Equal;
    }
}

// Default
impl<..T: Default> Default for [..T] {
    fn default() -> [..T] {
        return [..T::default()];
    }
}

// Serialize (hypothetical)
impl<..T: Serialize> Serialize for [..T] {
    fn serialize(&self, s: &mut Serializer) {
        s.begin_seq(..len(T));
        for let v of self {
            v.serialize(s);
        }
        s.end_seq();
    }
}
```

### Tuple Utilities

```wado
// Prepend a value to a tuple
fn prepend<..T>(head: i32, tail: [..T]) -> [i32, ..T] {
    return [head, ..tail];
}

// Concatenate two tuples
fn concat<..T, ..U>(a: [..T], b: [..U]) -> [..T, ..U] {
    return [..a, ..b];
}

// Check all elements satisfy a predicate
fn all_positive<..T: Ord + Default>(t: [..T]) -> bool {
    for let v of t {
        if !(v > T::default()) { return false; }
    }
    return true;
}

// Collect each element's string representation
fn to_strings<..T: Display>(t: [..T]) -> Array<String> {
    let mut result: Array<String> = [];
    for let v of t {
        result.append(`{v}`);
    }
    return result;
}
```

### Multi-Pack: Zip

```wado
// Element-wise pairing of two same-length tuples
fn zip<..T, ..U>(a: [..T], b: [..U]) -> Array<[i32, i32]> {
    // Direct element-wise return type [..[T, U]] is deferred to future work.
    // For now, zip where T and U are both constrained:
    let mut result: Array<String> = [];
    for let [i, v] of a.enumerate() {
        let w = b[i];
        result.append(`({v}, {w})`);
    }
    return result;
}
```

Note: returning `[..[T, U]]` (a tuple of pairs, where each pair's types come from the two
packs) requires element-wise pack pairing, which is deferred to a future WEP extension.

### Tagged Template Upgrade

The existing `sql` tag from WEP-2026-02-10 works unchanged, but gains definition-time
bounds checking:

```wado
// Before: Values is a single opaque generic type T
// After: Values is a pack with bounds, enabling early error messages
fn sql<..Values: ToSqlParam>(strings: CookedStrings, values: [..Values]) -> SqlQuery {
    let mut query = strings[0];
    let mut params: Array<SqlParam> = [];
    for let [i, v] of values.enumerate() {
        params.append(v.to_sql_param());
        query.append("?");
        query.append(strings[i + 1]);
    }
    return SqlQuery { query, params };
}
```

With `..Values: ToSqlParam`, if a template argument does not implement `ToSqlParam`, the
error points to the specific argument and element type rather than the monomorphization
location.

---

## What is NOT in Scope

The following are deliberately deferred:

- **Element-wise pack mapping with type transformation** (`fn map<..T, ..U>(t: [..T], f: fn(T) -> U) -> [..U]`):
  requires declaring a functional relationship between two packs, significant design work.

- **`[..[T, U]]` return type** (zip returning a tuple of pairs): a special "repeat-pattern"
  syntax akin to Swift's `repeat (each T, each U)`. Deferred until multi-pack use cases are
  clearer.

- **Type-level pack operations**: `Head<..T>`, `Tail<..T>`, `Nth<..T, N>` — type-level list
  manipulation. Can be addressed when concrete need arises.

- **Pack splat in function call arguments**: `f(..args)` — spreading a tuple as positional
  arguments to a function expecting individual parameters. Separate feature.

---

## Implementation Plan

### Compiler phases affected

1. **Parser**: Recognize `..T` in generic parameter lists and `[..T]` in type positions.
   Recognize `[..a, ..b]` and `[..T::method()]` in tuple literal expressions.
   Recognize `..len(T)` as a compile-time length expression.

2. **AST / TIR**: Add `TypePackParam` node for `..T` declarations. Add `PackSpread` node for
   `[..a]` and `[..T::method()]`. Store pack bounds alongside element-type bounds.

3. **Type resolver**: Verify pack bounds at call sites. Check multi-pack shape constraints.
   Propagate pack types through `[..T]` positions in return types.

4. **Monomorphizer**: When a pack parameter is substituted with a concrete tuple type,
   expand `for let v of tuple` loop bodies (already done by WEP-2026-02-10), expand
   `[..a, ..b]` into concrete tuple literals, expand `[..T::method()]` into per-element
   calls.

5. **Error messages**: When a bound `..T: Trait` is not satisfied, report the specific
   element index and type that failed, plus the call site.

### Phasing

- **Phase 1** (this WEP): single type pack `..T`, bounds `..T: Trait`, `[..T]` in types,
  `[..a, ..b]` spread in expressions, `[..T::method()]` pack construction, `..len(T)`.
  Implement `Eq`, `Ord`, `Default` for tuples in stdlib.

- **Phase 2** (future WEP): multi-pack same-shape functions (zip, element-wise operations),
  `[..[T, U]]` return types.

- **Phase 3** (future WEP): type-level pack manipulation, pack splat in function calls.

---

## Consequences

### Positive

- **Eliminates per-arity boilerplate**: One `impl<..T: Eq> Eq for [..T]` replaces a
  potentially unbounded sequence of per-arity impls.
- **Compositional**: works with existing trait bounds, compile-time tuple enumeration, and
  monomorphization — no new runtime mechanisms.
- **Consistent syntax**: `..` is already the Wado idiom for "rest / spread". Extending it to
  type parameters and expressions follows the same mental model.
- **Zero runtime overhead**: all expansion happens at compile time; generated Wasm is
  identical to hand-written per-arity code.
- **Enables tuple-as-first-class-collection**: `Eq`, `Ord`, `Default`, `Serialize`,
  `Deserialize` can all be derived once for tuples of any length.

### Negative

- **Monomorphization code size**: each distinct pack instantiation produces a separate copy.
  Large packs or many distinct instantiations can increase binary size. (Mitigated by the
  Wasm optimizer and `-Os`.)
- **Error messages at monomorphization**: when a bound is violated, the error points to the
  call site rather than the definition site (same trade-off as WEP-2026-02-10's C++ template
  model). Extra effort is needed to produce helpful error messages.
- **Parser and resolver complexity**: pack parameters require new AST nodes and
  monomorphization-time expansion logic.
- **Multi-pack shape errors**: two packs that must match in length will produce errors that
  can be confusing without careful engineering.

### Risks

- **Complexity creep**: variadic parameters create pressure for type-level pack computations
  (length arithmetic, head/tail, etc.). The phase structure is designed to resist this, but
  user demand may push earlier.
- **Interaction with future features**: effects, closures, and async complicate variadic
  design (e.g., `fn map<..T, ..U>(t: [..T], f: fn(each T) -> each U) -> [..U]` requires
  element-wise closure types, which is a deep feature).

---

## Related WEPs

- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md) —
  the iteration primitive that variadic impl bodies rely on; no changes needed to it.
- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) — defines
  `[T, U]` tuple types and literals that packs extend.
- [Tuple Destructuring](./wep-2026-02-22-tuple-destructuring.md) — `..` rest pattern in
  destructuring; semantically distinct from `..T` pack spread, but shares the `..` sigil.
- [Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md) — pack bounds `..T: Trait`
  follow the same enforcement model.
- [Default Trait](./wep-2026-03-04-default-trait.md) — `impl<..T: Default> Default for [..T]`
  is a primary motivating application.

## References

- [C++ Parameter Pack — cppreference.com](https://en.cppreference.com/w/cpp/language/parameter_pack)
- [C++20 Idioms for Parameter Packs — Stanford](https://www.scs.stanford.edu/~dm/blog/param-pack.html)
- [TypeScript 4.0: Variadic Tuple Types](https://www.typescriptlang.org/docs/handbook/release-notes/typescript-4-0.html)
- [Swift SE-0393: Parameter Packs](https://github.com/swiftlang/swift-evolution/blob/main/proposals/0393-parameter-packs.md)
- [A madman's guide to variadic generics — Rust Internals](https://internals.rust-lang.org/t/a-madmans-guide-to-variadic-generics/19605)
- [Draft RFC: variadic generics — rust-lang/rfcs #376](https://github.com/rust-lang/rfcs/issues/376)
- [Variadic Generics ideas that won't work for Rust — PoignardAzur](https://poignardazur.github.io/2025/07/09/variadic-generics-dead-ends/)
- [HList: Heterogeneous Lists — Hackage](https://hackage.haskell.org/package/HList)

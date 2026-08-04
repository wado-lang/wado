# WEP: Variadic Type Parameters

## Context

Wado's tuple type `[T, U, V]` is heterogeneous and fixed-arity. Today, writing a trait
implementation that covers tuples of **any** length requires per-arity boilerplate, much
like Rust's standard library does with procedural macros for arities up to 12. This is
the problem variadic type parameters solve.

Primary use cases driving this design:

1. **Tuple trait impls without arity explosion**: `Eq`, `Default`, `Clone`, `Inspect`,
   `Serialize` for any tuple without writing separate impls for 0, 1, 2, … element tuples.
2. **Struct reflection**: expose a struct's fields as a typed tuple at compile time so that
   generic Wado code (rather than compiler magic) can implement `Inspect` for structs.

Background research: [Research: Variadic Generics / Variadic Templates](./research-variadic-generics.md)

---

## Decision

### 1. Type Pack Declaration

A type parameter prefixed with `..` declares a **type pack** — a sequence of zero or more
types that is fixed at each monomorphization site:

```wado
fn process<..T>(values: [..T]) { }
```

`..T` is a single parameter that stands for any number of type arguments. Multiple scalar
generic parameters and at most one type pack may appear together:

```wado
fn example<A, B, ..T>(a: A, b: B, rest: [..T]) { }
```

The `..` prefix was chosen for consistency with Wado's existing `..` semantics: struct
update (`..p`), rest patterns (`[a, ..]`), and value spread (`[..a, ..b]`). All uses of
`..` in Wado carry the meaning "expand / spread a sequence."

### 2. Type Pack in Type Position

A type pack `..T` may appear inside `[...]` to produce a tuple type:

```wado
[..T]          // a tuple whose element types are the pack T
[i32, ..T]     // an i32 followed by the elements of T
[..T, ..U]     // concatenation of two packs (see §6 Multi-Pack)
```

This mirrors tuple literal syntax and tuple destructuring syntax, making `..T` in a type
context visually consistent with its value-context meaning.

### 3. Tuple Type Declaration in Prelude

To establish a clear owner for the tuple type family, the prelude declares:

```wado
pub type [...T];
```

This declaration names the module that owns tuple types as `core:prelude`. Without this,
orphan rules (see §5) cannot determine whether a variadic impl is in the "right" crate.
The declaration itself generates no code; it is a type-system anchor.

### 4. Bounds on Type Packs

A pack parameter may carry trait bounds using the same `..T: Trait` syntax as scalar bounds:

```wado
fn inspect_all<..T: Inspect>(values: [..T]) -> List<String> { ... }

impl<..T: Eq> Eq for [..T] { ... }

impl<..T: Default> Default for [..T] { ... }
```

The bound `..T: Trait` means "every type in the pack T implements Trait." This is checked
at monomorphization: when `T` is instantiated to `[i32, String]`, the compiler verifies
that `i32: Eq` and `String: Eq`.

Multiple bounds are written with `+`:

```wado
impl<..T: Clone + Eq> CloneAndEq for [..T] { ... }
```

### 5. Coherence Rules

Two rules govern impl overlap for variadic impls:

**Rule 1 — Non-variadic wins**: When both a non-variadic impl and a variadic impl could
apply to a concrete type, the non-variadic impl takes priority. This allows a concrete
specialization to override the general tuple impl:

```wado
impl Eq for [i32, i32] { ... }        // concrete — wins over ↓
impl<..T: Eq> Eq for [..T] { ... }    // variadic — fallback
```

**Rule 2 — Variadic overlap is forbidden**: Two variadic impls for the same trait and same
head type are a compile error at definition time:

```wado
impl<..T: Eq>  Eq for [..T] { ... }   // OK
impl<..T: Ord> Eq for [..T] { ... }   // ERROR: overlapping variadic impls
```

These two rules together mirror the priority model used in WEP-2026-02-10 for tuple
enumeration and keep the coherence model simple without a full trait solver overhaul.

Rule 1 needs no priority table. A concrete tuple impl is a concrete _instantiation_
impl — the same shape as `impl Tag for List<u8>` — so it defines the very function a
`[i32, i32]` receiver calls, and the template is not instantiated onto a name an impl
already occupies. The specific impl wins because it got there first.

Rule 1 is scoped to one trait in both directions. It ranks a trait's own impls against
each other and says nothing about another trait's, so a foreign blanket `impl<T> A for T`
must not outrank a local `impl<..T> B for [..T]`. And it needs the trait's signature to
make the specific and general methods interchangeable, so it does not extend to inherent
impls: `impl<T> Box_<T> { fn a() -> String }` beside `impl Box_<i32> { fn a() -> i32 }`
is a duplicate definition, rejected as in Rust, rather than a specialization that would
relink a generic caller to a differently-typed function.

Rule 2 is a definition-time check: two variadic impls of one trait apply at every arity,
and a pack's bounds are resolved only at monomorphization, so nothing could separate them
at selection time. The trait's own _arguments_ do separate them — `Conv<i32>` and
`Conv<String>` are implementations of different things.

A variadic impl target must be the bare `[..T]`. `[i32, ..T]` is a legal type (§2) but
not yet a legal impl target.

Orphan rules apply normally: a variadic impl `impl<..T> Trait for [..T]` is only legal
if either `Trait` or the tuple type family (`type [...T]` from `core:prelude`) is owned
by the current crate. Because `core:prelude` owns tuples, the standard library can write
variadic tuple impls; downstream crates may write variadic impls only for their own traits.

### 6. Multi-Pack (Limited)

Two packs may appear in the same impl or function only in a type-level position (not in a
single expansion context). The primary use case is concatenation:

```wado
fn concat<..A, ..B>(a: [..A], b: [..B]) -> [..A, ..B] { ... }
```

When two packs appear in an expansion expression (§8), they must have the same length at
every call site; this is enforced at monomorphization time. More complex multi-pack
operations (zip, interleave) are out of scope for this WEP.

### 7. Compile-Time Tuple Enumeration with Packs

The existing compile-time tuple enumeration (WEP-2026-02-10) works unchanged when the
tuple type is `[..T]` — once the pack is instantiated to a concrete tuple type, the
elaborator unrolls the `for let v of tuple` loop as usual:

```wado
fn inspect_all<..T: Inspect>(values: [..T]) -> List<String> {
    let mut parts: List<String> = [];
    for let v of values {
        parts.push(v.inspect());
    }
    return parts;
}
```

No changes to the existing `for let v of tuple` semantics are required. The only new
requirement is that the monomorphizer recognizes `[..T]` as a concrete tuple type once
`T` is substituted.

### 8. Expansion Syntax

Two syntactic forms exist for constructing a new tuple from a type pack:

#### 8a. Type Pack Expansion: `[..T::method()]`

When there is no source value and construction is driven purely by the type pack, use
`..T::method()` inside a tuple literal:

```wado
impl<..T: Default> Default for [..T] {
    fn default() -> [..T] {
        return [..T::default()];
    }
}
```

`[..T::default()]` expands at monomorphization to `[T_0::default(), T_1::default(), ...]`
— one call per type in the pack.

#### 8b. Value-Transform Collection: `[for let v of tuple { expr }]`

When a source tuple exists and each element is transformed to produce the result tuple,
wrap a `for let v of tuple` expression in `[...]`:

```wado
impl<..T: Clone> Clone for [..T] {
    fn clone(&self) -> [..T] {
        return [for let v of *self { v.clone() }];
    }
}
```

At monomorphization, the compiler unrolls the loop and collects each result expression
into the corresponding position of a new tuple literal. The result type is `[..T]` when
`v` has type `T_k` and the body expression has type `T_k`.

The `[for let [i, v] of tuple.enumerate() { expr }]` form (with index binding) is also
valid.

**Disambiguation with arrays**: `[for let v of x { expr }]` produces a tuple when `x`
has a tuple type (known at monomorphization time) and an array when `x` has type
`List<E>` (runtime iteration). The two paths are resolved by the type of the iterable.

**Break/continue** inside `[for ... { }]` are compile errors, consistent with WEP-2026-02-10.

### 9. Value Spread

A type pack value `a: [..T]` can be spread into a tuple literal using `..a`:

```wado
fn prepend<H, ..T>(head: H, tail: [..T]) -> [H, ..T] {
    return [head, ..tail];
}
```

`[..a, ..b]` concatenates two pack values into one tuple, consistent with existing struct
update spread (`..p`) and the general "spread a sequence" meaning of `..`.

### 10. Reflect: Struct Metadata as a Typed Tuple

`ReflectStruct` is a **compiler-synthesized**, sealed language feature — it cannot be implemented
in user code. It exposes a struct's field types and members at compile time via a trait, and
its members are reached only as `ReflectStruct::<T>::members()` (see
[Reflect Derivation](./wep-2026-06-13-reflect-derivation.md)):

```wado
#[compiler_item("reflect_struct")]
internal trait ReflectStruct {
    type FieldTypes;
    type Members;
    fn members() -> Self::Members;
    fn type_name() -> String;
    fn wire_name_policy() -> CaseStyle;
}
```

The compiler automatically synthesizes `impl ReflectStruct for S` for every struct `S`. For a
struct with fields `f_0: F_0, f_1: F_1, …`:

- `type FieldTypes = [F_0, F_1, …]` — the payload pack, bound to place per-field bounds
- `type Members = [StructField<S, F_0>, StructField<S, F_1>, …]`
- `members()` returns one `StructField` per field, carrying its name, attributes and a
  `get(&self, v: &S) -> F_k` value accessor
- `type_name()` returns the struct name as a string

**Why compiler-synthesized**: `ReflectStruct` returns `Self::Members`, which is a concrete tuple
type specific to each struct. Without `any`, the compiler must generate the implementation
at compile time for each struct individually.

**Why only in monomorphized contexts**: `ReflectStruct::<T>::members()` and
`ReflectStruct::<T>::type_name()` are only callable when `T` is a concrete struct type, because
the implementation is generated per struct, not for a generic `T`.

### 11. `where` Clause — Type Pack Pattern Matching

A `where` clause may bind a type pack from an associated type:

```wado
impl<T, ..F: Inspect> Inspect for T
where T: ReflectStruct<FieldTypes = [..F]>
{
    fn inspect(&self) -> String {
        let mut parts: List<String> = [];
        for let f of ReflectStruct::<T>::members() {
            parts.push(`${f.name()}: ${f.get(self).inspect()}`);
        }
        return `${ReflectStruct::<T>::type_name()} \{ ${parts.join(", ")} \}`;
    }
}
```

`T: ReflectStruct<FieldTypes = [..F]>` constrains `T` to be any type that implements `ReflectStruct`
with a `Fields` associated type that matches the pack `F`. The compiler extracts `F` from
the concrete `Fields` type at monomorphization. This is the mechanism that lets the
struct-inspect implementation be written entirely in Wado.

---

## Type Checking Model

Variadic generics follow the same **C++ template model** used by compile-time tuple
enumeration (WEP-2026-02-10): type-checking occurs at monomorphization time, not at
definition time.

At definition time the compiler:

- Parses and stores pack declarations and bounds
- Does not verify that pack element types satisfy method calls in the body
- Does verify structural well-formedness (e.g., that `..T` is used in a valid position)

At monomorphization time the compiler:

- Substitutes concrete types for the pack
- Unrolls `for let v of tuple` and `[for let v of tuple { }]` loops
- Type-checks each unrolled block independently
- Checks trait bounds on each concrete element type

Error messages must include: the call site where the concrete pack was determined, the
specific element index and type that failed, and the location in the body where the error
occurred.

---

## Standard Library Applications

### Tuple `Eq`

```wado
impl<..T: Eq> Eq for [..T] {
    fn eq(&self, other: &Self) -> bool {
        let mut result = true;
        for let [i, v] of (*self).enumerate() {
            if !v.eq(&(*other)[i]) { result = false; }
        }
        return result;
    }
}
```

### Tuple `Default`

```wado
impl<..T: Default> Default for [..T] {
    fn default() -> [..T] {
        return [..T::default()];
    }
}
```

### Tuple `Clone`

```wado
impl<..T: Clone> Clone for [..T] {
    fn clone(&self) -> [..T] {
        return [for let v of *self { v.clone() }];
    }
}
```

### Tuple `Inspect`

```wado
impl<..T: Inspect> Inspect for [..T] {
    fn inspect(&self) -> String {
        let mut parts: List<String> = [];
        for let v of *self {
            parts.push(v.inspect());
        }
        return `[${parts.join(", ")}]`;
    }
}
```

### Tuple `Serialize`

```wado
impl<..T: Serialize> Serialize for [..T] {
    fn serialize<S: Serializer>(&self, seq: &mut S::SeqSerializer) -> Result<(), SerializeError> {
        for let v of *self {
            seq.element(&v)?;
        }
        return Result::<(), SerializeError>::Ok(());
    }
}
```

### Tuple `Deserialize`

```wado
impl<..T: Deserialize> Deserialize for [..T] {
    fn deserialize<D: Deserializer>(seq: &mut D::SeqAccess) -> Result<[..T], DeserializeError> {
        return Result::<[..T], DeserializeError>::Ok([for let _v of [..T::default()] {
            seq.next_element()?
        }]);
    }
}
```

### Struct `Inspect` via `ReflectStruct`

```wado
impl<T, ..F: Inspect> Inspect for T
where T: ReflectStruct<FieldTypes = [..F]>
{
    fn inspect(&self) -> String {
        let mut parts: List<String> = [];
        for let f of ReflectStruct::<T>::members() {
            parts.push(`${f.name()}: ${f.get(self).inspect()}`);
        }
        return `${ReflectStruct::<T>::type_name()} \{ ${parts.join(", ")} \}`;
    }
}
```

---

## Implementation Plan

- [ ] The `[for let v of tuple { expr }]` / `[for let [i, v] of tuple.enumerate() { expr }]`
      construction form: unparsed. Until it lands, a build derivation needing the
      member handle or index per element routes the work through a pack map
      (`[..F::method(args)]`) over a pre-ordered cursor. What waits on it:
      [the struct build direction over a streaming format](./wep-2026-06-13-reflect-derivation.md#building-a-struct-from-a-streaming-format)
- [ ] `.enumerate()` over a variadic `for-of`. The concrete-tuple form expands
      it; the deferred (`VariadicForOf`) one rejects it outright
- [ ] The `.enumerate()` index as a tuple subscript. It is a compile-time
      constant by construction, but `slots[i]` is rejected as a non-constant
      index
- [ ] Variadic impl targets other than the bare `[..T]` — fixed elements
      (`[i32, ..T]`) or under a reference (`&[..T]`): rejected for now.
      Selection, pack binding, and template naming all ignore the fixed
      elements, and a pack under a reference never reaches the impl's
      type-param scope
- [ ] `where` clause pack binding: parse `T: Trait<Assoc = [..F]>` and extract `F`
- [ ] Error messages: show call site, element index, and body location
- [ ] Standard library: add variadic impls for `Default`, `Clone`

---

## Consequences

### Positive

- Tuple trait impls are written once for all arities — no per-arity boilerplate
- Struct `Inspect` moves from compiler magic to ordinary Wado code
- Expansion syntax (`..T::method()`, `[for let v { }]`) is minimal and consistent with
  existing Wado conventions
- Coherence model is simple: two rules, no full trait solver rewrite
- Zero runtime cost: all expansion happens at compile time via monomorphization

### Negative

- Monomorphization-time errors are harder to attribute than definition-time errors; error
  message quality requires careful engineering
- Each unique pack instantiation generates separate code (binary size growth for large
  tuples or many distinct instantiations)
- `ReflectStruct` is compiler-synthesized; it cannot be manually implemented or overridden by
  user code

### Out of Scope (Future Work)

- **Pack indexing**: `T[0]` to extract the first type from a pack — useful but complex
- **Definition-time trait bound checking**: checking `..T: Trait` at definition time
  rather than at monomorphization (requires a richer trait solver)
- **Fold operations**: `(v op ... op init)` C++-style fold expressions
- **Complex multi-pack operations**: zip, interleave of packs with different lengths
- **Variadic closures**: `|..args: ..T|` — deferred until closure monomorphization is
  well understood

---

## See Also

- [Research: Variadic Generics / Variadic Templates](./research-variadic-generics.md)
- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)
- [Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md)
- [Default Trait](./wep-2026-03-04-default-trait.md)
- [Inspect / Debug Output](./wep-2026-02-21-inspect-debug-output.md)
- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
- [Tuple Destructuring](./wep-2026-02-22-tuple-destructuring.md)
- [Serialization and Deserialization](./wep-2026-02-28-serde.md)

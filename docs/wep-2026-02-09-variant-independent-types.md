# WEP: Variant-Independent Types

## Context

Wado's variant payload design (`wep-2026-01-25-variant-payload-design.md`) proposes that each variant case should be usable as an independent type:

```wado
variant Shape {
    Circle(f64),
    Rectangle([f64, f64]),
    Point,
}

fn process_circle(c: Shape::Circle) {
    println(`radius: {c.0}`);
}
```

This was listed as "Phase 3" with no detailed semantics. This WEP defines the full semantics, including the subtype relationship, method resolution, trait implementations, and operator overloading, and establishes that **all dispatch is static** — no virtual method tables or runtime dispatch is introduced.

### Motivation

Rust's enums do not allow treating individual variants as types. This leads to:

- Functions that accept a full enum but only handle one variant, requiring `unreachable!()` in other arms
- Loss of type information after construction
- No way to statically guarantee a function only receives a specific variant

TypeScript's discriminated unions treat each member as a standalone type. Wado can do the same while maintaining static dispatch.

### Wasm GC Foundation

The SubtypeHierarchy representation already provides the necessary foundation:

```wat
(type $Shape          (struct (field $tag i32)))
(type $Shape::Circle  (sub $Shape (struct (field $tag i32) (field $payload f64))))
(type $Shape::Point   (sub $Shape (struct (field $tag i32))))
```

`$Shape::Circle` is already a Wasm GC subtype of `$Shape`. The upcast is zero-cost — the reference is used as-is with a wider type. Downcast uses `ref.test` / `ref.cast`.

For NullableRef variants (e.g., `Option<T>` with 2 cases, 1 unit):

- The payload case type is the non-null reference type
- The unit case type is the null singleton
- Upcast widens from non-nullable to nullable

## Decision

### 1. Core Principle: Static Dispatch Only

The subtype relationship between variant cases and their parent variant is for **data compatibility (upcast) only**, not for behavioral polymorphism (virtual dispatch).

Concretely:

- `Shape::Circle` can be implicitly coerced to `Shape` (upcast)
- Calling a method on `Shape` uses `Shape`'s method table, never dispatching to case-specific implementations
- Calling a method on `Shape::Circle` uses `Shape::Circle`'s method table
- There is no implicit virtual dispatch through the variant type

This is consistent with Wado's existing static dispatch model and with Wasm GC's type system, which provides subtype testing (`ref.test`) and casting (`ref.cast`) but not virtual method dispatch.

### 2. Variant Case as Type

Each variant case can be used as a type:

```wado
// As function parameter
fn area(c: Shape::Circle) -> f64 {
    return 3.14159 * c.0 * c.0;
}

// As variable type
let circle: Shape::Circle = Shape::Circle(5.0);

// As return type
fn make_circle(r: f64) -> Shape::Circle {
    return Shape::Circle(r);
}
```

For generic variants, the case type includes the type parameters:

```wado
let some: Option::<i32>::Some = Option::<i32>::Some(42);
let ok: Result::<i32, String>::Ok = Result::<i32, String>::Ok(42);
```

Type inference is encouraged over explicit turbofish:

```wado
let some = Option::Some(42);          // inferred as Option::<i32>::Some
let opt: Option<i32> = some;          // upcast
```

### 3. Subtype Relationship

`Shape::Circle <: Shape` — every case type is a subtype of its parent variant type.

Implicit coercion (upcast):

```wado
let circle: Shape::Circle = Shape::Circle(5.0);
let shape: Shape = circle;  // OK: implicit upcast
```

Explicit downcast via pattern matching:

```wado
let shape: Shape = get_shape();
if let Circle(r) = shape {
    // r: f64 — extracted payload
    // To get Shape::Circle, reconstruct or use a typed binding
}
```

No implicit downcast. The only way to go from `Shape` to `Shape::Circle` is pattern matching.

### 4. Method Resolution

#### Methods on the variant type

```wado
impl Shape {
    fn name(&self) -> String {
        match self {
            Circle(_) => "circle",
            Rectangle(_) => "rectangle",
            Point => "point",
        }
    }
}
```

Callable on `Shape` and on any case type (because case types are subtypes):

```wado
let circle = Shape::Circle(5.0);
circle.name();   // OK: Shape::Circle <: Shape, calls Shape::name
```

The call works by implicitly upcasting `&Shape::Circle` to `&Shape`. The method body receives `&Shape` and uses pattern matching internally. This is static dispatch — no vtable.

#### Methods on a case type

```wado
impl Shape::Circle {
    fn area(&self) -> f64 {
        return 3.14159 * self.0 * self.0;
    }
}
```

Only callable when the static type is `Shape::Circle`:

```wado
let circle = Shape::Circle(5.0);
circle.area();   // OK

let shape: Shape = circle;
// shape.area();  // ERROR: Shape has no method 'area'
```

To call case-specific methods through a variant reference, use pattern matching:

```wado
if let Circle(r) = shape {
    let c = Shape::Circle(r);
    c.area();
}
```

#### Lookup order for case type method calls

When calling a method on `Shape::Circle`:

1. Look for methods defined on `Shape::Circle` (case-specific `impl`)
2. Look for methods defined on `Shape` (parent variant `impl`, via upcast)

If both define the same method name, the case-specific method takes priority.

### 5. Trait Implementations

#### Trait impl for a case type

```wado
impl Eq for Shape::Circle {
    fn eq(&self, other: &Self) -> bool {
        return self.0 == other.0;
    }
}
```

This makes `Shape::Circle` satisfy the `Eq` bound. It does **not** make `Shape` satisfy `Eq`.

#### Trait impl for the variant type

```wado
impl Eq for Shape {
    fn eq(&self, other: &Self) -> bool {
        match [self, other] {
            [Circle(r1), Circle(r2)] => r1 == r2,
            [Point, Point] => true,
            _ => false,
        }
    }
}
```

This makes `Shape` satisfy the `Eq` bound.

#### Trait fallback from variant to case

If `impl Eq for Shape` exists but `impl Eq for Shape::Circle` does not, then `Shape::Circle` inherits `Eq` from `Shape` (the case value is upcasted to `Shape` for the method call).

If both exist, the case-specific impl takes priority when the static type is `Shape::Circle`.

```wado
// Resolution rules:
// 1. Does Shape::Circle have its own impl Eq?  → use it
// 2. Does Shape have impl Eq?                  → use it (via upcast)
// 3. Neither                                   → Eq not satisfied
```

This is analogous to newtype trait inheritance, but in the opposite direction (subtype inherits from supertype, not derived from base).

#### No automatic composition

Case-level trait impls do **not** automatically compose into a variant-level impl:

```wado
impl Eq for Shape::Circle { ... }
impl Eq for Shape::Point { ... }
// This does NOT mean Shape implements Eq
// You must write impl Eq for Shape { ... } explicitly
```

This avoids the need for dynamic dispatch. If case impls automatically composed into a variant impl, calling `shape.eq(other)` on a `Shape` reference would require runtime dispatch to determine which case's `eq` to call.

### 6. Operator Overloading

Operator traits work identically to regular traits:

```wado
impl Add for Shape::Circle {
    type Output = Shape::Circle;
    fn add(self, other: Shape::Circle) -> Shape::Circle {
        return Shape::Circle(self.0 + other.0);
    }
}

let c1 = Shape::Circle(3.0);
let c2 = Shape::Circle(4.0);
let c3 = c1 + c2;  // Shape::Circle(7.0) — static dispatch
```

This does not make `Shape + Shape` valid. To support `Shape + Shape`, write `impl Add for Shape` with pattern matching inside.

Mixing case types in operators is only possible if explicitly defined:

```wado
// c1 + c2 where c1: Shape::Circle, c2: Shape::Circle → OK (if impl Add for Shape::Circle)
// s1 + s2 where s1: Shape, s2: Shape → OK only if impl Add for Shape
// c1 + s1 where c1: Shape::Circle, s1: Shape → OK only if impl Add for Shape (c1 upcasted)
```

### 7. Interaction with Pattern Matching

When the parameter type is a case type, pattern matching is unnecessary:

```wado
fn circle_area(c: Shape::Circle) -> f64 {
    return 3.14159 * c.0 * c.0;  // Direct payload access
}
```

Pattern matching on a case type still works but is trivially exhaustive:

```wado
fn process(c: Shape::Circle) {
    match c {
        Circle(r) => println(`radius: {r}`),
        // No other arms needed — exhaustive
    }
}
```

### 8. NullableRef Variant Cases

For variants using the NullableRef representation (e.g., `Option<T>`):

| Case Type | Wasm Representation | Values |
|-----------|-------------------|--------|
| `Option::<T>::Some` | `(ref $T)` or `(ref $box_T)` | non-null reference |
| `Option::<T>::None` | unit type | single value (null) |

The `None` case type is effectively a unit type with one value. Implementing traits on it is allowed but rarely useful.

```wado
// Allowed but unusual
impl Display for Option::<i32>::None {
    fn display(&self) -> String {
        return "None";
    }
}
```

### 9. Generic Variant Case Types

For generic variants, case types carry the type parameters:

```wado
variant Result<T, E> {
    Ok(T),
    Err(E),
}

// Monomorphized case types
fn unwrap(r: Result::<i32, String>::Ok) -> i32 {
    return r.0;
}
```

Type parameter inference works when context provides the information:

```wado
let ok = Result::Ok(42);  // Result::<i32, ???>::Ok — E not determined
let ok: Result<i32, String> = Result::Ok(42);  // Full type known
```

### 10. Comparison with Related Features

| Aspect | Newtype (`type T = U`) | Variant Case (`V::Case`) |
|--------|----------------------|--------------------------|
| Direction | `T` derived from `U` | `V::Case <: V` |
| Conversion | Explicit `as` both directions | Implicit upcast, pattern match downcast |
| Method inheritance | Inherits all from base | Inherits from parent variant |
| Own methods | `impl T { }` | `impl V::Case { }` |
| Trait inheritance | All base traits inherited | Parent variant traits as fallback |
| Runtime cost | Zero (same representation) | Zero (Wasm GC subtyping) |

| Aspect | Trait objects (`&dyn Trait`) | Variant case types |
|--------|---------------------------|-------------------|
| Dispatch | Dynamic (vtable) | Static |
| Type erasure | Yes | No |
| Open/closed | Open (any implementor) | Closed (fixed set of cases) |
| Use case | Heterogeneous collections | Type-safe variant handling |

## Implementation Strategy

### Phase 1: Type System

- Parse `Shape::Circle` as a type in type positions
- Add `ResolvedType::VariantCase { variant_name, case_name, module_source }` to the type system
- Implement subtype checking: `VariantCase <: Variant`
- Implement implicit upcast coercion in assignments and function calls

### Phase 2: Methods and Traits

- Allow `impl Shape::Circle { }` blocks
- Method resolution: case-specific methods first, then parent variant methods
- Allow `impl Trait for Shape::Circle { }` blocks
- Trait satisfaction: case-specific impl first, then parent variant impl as fallback

### Phase 3: Codegen

- For SubtypeHierarchy: case type maps directly to the existing per-case Wasm struct type
- For NullableRef: case types map to non-null ref (payload case) or unit (unit case)
- Method calls on case types compile to direct calls (no vtable)
- Upcast is a no-op in Wasm (subtype reference is already valid as parent type)

### Phase 4: Payload Access

- `self.0` on a case type accesses the payload field directly
- For SubtypeHierarchy: `struct.get` on the payload field of the case struct
- For NullableRef: unbox if needed

## Consequences

### Benefits

- Functions can accept specific variant cases, improving type safety
- No `unreachable!()` arms in pattern matching when the variant case is known
- Static dispatch throughout — no runtime overhead
- Natural fit with Wasm GC's subtype hierarchy
- Operator overloading works for specific cases without affecting the variant type
- Consistent with Wado's existing static dispatch model

### Trade-offs

- Upcasting changes method resolution: `circle.case_method()` works but after `let shape: Shape = circle`, `shape.case_method()` does not — this is intentional but may surprise users coming from OOP languages
- Case-level trait impls do not compose into variant-level impls — explicit variant-level impls are required, which is more verbose but avoids dynamic dispatch
- NullableRef unit case types (e.g., `Option::<T>::None`) are somewhat degenerate types with a single value

### Why Not Dynamic Dispatch?

One might expect that if `Shape::Circle <: Shape`, then calling a method through `Shape` should dispatch to the case's implementation. This would require either:

1. **Vtable**: A function pointer table stored in each variant value, adding memory overhead and preventing Wasm GC optimizations
2. **`br_on_cast` chain**: A series of runtime type tests, linear in the number of cases

Both options add runtime cost and complexity. More importantly, they would make Wado's variant system behave like single-inheritance OOP, which contradicts the language's design philosophy of explicit control flow and static dispatch.

Instead, Wado provides two explicit alternatives:

- **Pattern matching**: For case-specific behavior through a variant reference, match and then call methods on the specific case
- **Trait objects** (`&dyn Trait`): For open polymorphism with dynamic dispatch, when truly needed (future feature)

This keeps variant types as what they fundamentally are — tagged unions with typed members — rather than class hierarchies with virtual methods.

## See Also

- [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md) — payload forms and Phase 3 roadmap
- [Variant Wasm GC Representation](./wep-2026-02-08-variant-representation.md) — NullableRef vs SubtypeHierarchy
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) — traits and impl blocks
- [Newtype Semantics](./wep-2026-01-29-newtype-semantics.md) — related concept of derived types
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md) — trait-based operators
- [Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md) — bounded impl blocks

# WEP: Variant Payload Design

## Context

Rust's enum payload system has been criticized for being ad-hoc:

1. **Three variant forms**: unit, tuple, and struct variants with subtly different behaviors
2. **Implicit tuple expansion**: `Foo(a, b)` vs `Foo((a, b))` distinction is confusing
3. **Variants are not types**: Cannot write `fn process(c: Shape::Circle)` directly
4. **Inconsistencies**: Unit vs empty tuple vs empty struct have different initialization rules

Wado's variant system should learn from these issues and provide a cleaner, more consistent design.

### TypeScript's Approach

TypeScript uses discriminated unions with literal types:

```typescript
type Shape =
    | { kind: "circle"; radius: number }
    | { kind: "rectangle"; width: number; height: number }
    | { kind: "point" };

// Each variant is a standalone type
type Circle = Extract<Shape, { kind: "circle" }>;

function processCircle(c: Circle) { ... }
```

This approach:

- Makes each variant a first-class type
- Uses structural typing for discrimination
- No special "variant" keyword needed

## Decision

### 1. Payload Forms

Wado variants support exactly four payload forms with **consistent syntax**:

| Form       | Syntax                    | Example                              | Description                           |
| ---------- | ------------------------- | ------------------------------------ | ------------------------------------- |
| **Unit**   | `Name`                    | `Point`                              | Sugar for `Name(())`, payload is `()` |
| **Scalar** | `Name(T)`                 | `Circle(f64)`                        | Single value                          |
| **Tuple**  | `Name([T, U, ...])`       | `Rectangle([f64, f64])`              | Explicit tuple type                   |
| **Struct** | `Name({ field: T, ... })` | `Named({ width: f64, height: f64 })` | Anonymous struct in parens            |

Key principles:

- **No implicit tuple expansion**: Multiple values require explicit tuple syntax `[T, U]` or struct syntax `{ a: T, b: U }`
- **Consistent wrapper**: All non-unit payloads use `Name(payload)` form
- **Minimal special rules**: Edge cases like single-element tuples `Foo([T])` or zero-element tuples `Foo([])` are allowed

```wado
variant Shape {
    Point,                                    // unit
    Circle(f64),                              // scalar
    Rectangle([f64, f64]),                    // explicit tuple
    Named({ width: f64, height: f64 }),       // struct in parens
}

// Construction
let p = Shape::Point;
let c = Shape::Circle(5.0);
let r = Shape::Rectangle([10.0, 20.0]);
let n = Shape::Named({ width: 10.0, height: 20.0 });
```

### 2. Variant Cases as Types

Each variant case is also a type that can be used independently:

```wado
variant Shape {
    Circle(f64),
    Rectangle([f64, f64]),
    Point,
}

// Shape::Circle is a type
fn process_circle(c: Shape::Circle) {
    println(`radius: ${c.0}`);
}

// Subtype relationship: Shape::Circle <: Shape
// Implicit coercion from variant case to variant
let circle = Shape::Circle(5.0);
let shape: Shape = circle;  // OK: implicit upcast
```

This enables:

- Functions that accept only specific variants
- No need for `unreachable!()` in match arms
- Better type safety and documentation

### 3. Turbofish Syntax for Generic Variants

The canonical syntax places type parameters on the variant type:

```wado
variant Option<T> {
    Some(T),
    None,
}

// Canonical: type parameter on variant, then case
let some: Option::<i32>::Some = Option::<i32>::Some(42);

// Recommended: rely on type inference
let some = Option::Some(42);  // inferred as Option::<i32>::Some
let opt: Option<i32> = some;  // confirms the type
```

Type inference is encouraged over explicit turbofish notation.

#### None and Polymorphic Unit Cases

For unit cases like `None` that don't use the type parameter, the type can be inferred from context:

```wado
// All equivalent:
let none: Option<i32> = Option::<i32>::None;  // canonical
let none: Option<i32> = Option::None;          // type from annotation
let none: Option<i32> = null;                  // null coerces to Option::None

// Error: T cannot be inferred
let none = Option::None;  // compile error
```

The `null` literal is a singleton value that implicitly coerces to `Option::None` for any `T`.

### 4. Pattern Matching

```wado
fn area(s: Shape) -> f64 {
    match s {
        Circle(r) => 3.14159 * r * r,
        Rectangle([w, h]) => w * h,
        Named({ width, height }) => width * height,
        Point => 0.0,
    }
}

// Full path also works in patterns
match s {
    Shape::Circle(r) => ...,
    Shape::Rectangle([w, h]) => ...,
    ...
}

// if let for single variant
if let Shape::Circle(r) = shape {
    println(`radius: ${r}`);
}

// When parameter type is Shape::Circle, no match needed
fn circle_area(c: Shape::Circle) -> f64 {
    return 3.14159 * c.0 * c.0;
}
```

### 5. Accessing Payload Fields

**Payload is always at `.0`** - this provides consistent access regardless of payload type:

| Payload Type | Access Syntax              | Example                           |
| ------------ | -------------------------- | --------------------------------- |
| Unit         | `.0`                       | `point.0` → `()`                  |
| Scalar       | `.0`                       | `circle.0` → the value            |
| Tuple        | `.0` then `.0`, `.1`, etc. | `rect.0.0`, `rect.0.1`            |
| Struct       | `.0` then `.field`         | `named.0.width`, `named.0.height` |

```wado
let c: Shape::Circle = Shape::Circle(5.0);
let radius = c.0;           // 5.0

let r: Shape::Rectangle = Shape::Rectangle([10.0, 20.0]);
let tuple = r.0;            // [f64, f64]
let width = r.0.0;          // 10.0
let height = r.0.1;         // 20.0

let n: Shape::Named = Shape::Named({ width: 10.0, height: 20.0 });
let payload = n.0;          // { width: f64, height: f64 }
let w = n.0.width;          // 10.0
```

In practice, pattern matching is preferred (99% of cases):

```wado
if let Shape::Rectangle([w, h]) = shape {
    println(`${w} x ${h}`);
}
```

## Union Type & Subset Binding

Beyond named variants, Wado supports anonymous union types with a powerful **subset binding** feature.

### Union Types

Union types are anonymous sum types with set semantics:

```wado
type A = { kind: "a", a: i32 };
type B = { kind: "b", b: i32 };
type C = { kind: "c", c: i32 };
type D = { kind: "d", d: i32 };

type AB = A | B;
type CD = C | D;
type ABCD = AB | CD;  // flattens to A | B | C | D
```

Internally, union types have a discriminant (tag) just like variants.

#### Union with Primitives

Union types work with any types, including primitives:

```wado
type IntOrString = i32 | String;

fn process(x: IntOrString) {
    if let n: i32 = x {
        println(`int: ${n}`);
    } else if let s: String = x {
        println(`string: ${s}`);
    }
}
```

#### Union of Variant Cases

Variant cases can be combined into union types:

```wado
variant Shape { Circle(f64), Rectangle([f64, f64]), Point }

type CircleOrRect = Shape::Circle | Shape::Rectangle;

fn process_non_point(s: CircleOrRect) {
    // Only Circle or Rectangle, Point is excluded at type level
}
```

### Normalization Rules

Union types follow set semantics with normalization:

```wado
// Flattening
type AB = A | B;
type BA = B | A;
type ABBA = AB | BA;      // normalizes to A | B (order: first occurrence)

// Duplicate handling - becomes Union<A>, not bare A
type AA = A | A;          // Union<A> - requires match binding to use

// Why Union<A> instead of A?
// Consistency: all union types require subset binding/match
// Edge case, but consistent rules are more important
```

### Subset Binding

**Subset binding** allows pattern matching against any subset of a union type:

```wado
fn process(x: ABCD) {
    // Bind to a named subset type
    if let ab: AB = x {
        // ab: A | B
        println("got A or B");
    }

    // Bind to an inline subset
    if let bc: B | C = x {
        // bc: B | C
        println("got B or C");
    }

    // Bind to a single type
    if let a: A = x {
        // a: A
        println(`got A with a=${a.a}`);
    }
}
```

Subset binding also works in match expressions:

```wado
match x {
    ab: A | B => println("A or B"),
    c: C => println("C"),
    d: D => println("D"),
}
```

This provides set-like operations that named variants cannot express:

| Feature                 | `variant` | Union Type     |
| ----------------------- | --------- | -------------- |
| Named cases             | ✓         | ✗ (structural) |
| Exhaustiveness checking | ✓         | ✓              |
| Subset binding          | ✗         | ✓              |
| Set operations          | ✗         | `\|` (union)   |

### Comparison with TypeScript

TypeScript's type narrowing:

```typescript
function process(x: A | B | C | D) {
    if (isAB(x)) {
        // x: A | B (via type guard)
    }
}
```

Wado's subset binding:

```wado
fn process(x: A | B | C | D) {
    if let ab: A | B = x {
        // ab: A | B (via subset binding)
    }
}
```

Wado's approach is more declarative - no need for manual type guard functions.

## Edge Cases

### Empty Variant

Empty variants are allowed (uninhabited type):

```wado
variant Empty {}  // valid, no values can exist
```

### Recursive Variants

Self-referential variants are supported:

```wado
variant List<T> {
    Cons({ head: T, tail: List<T> }),
    Nil,
}

let list = List::Cons({
    head: 1,
    tail: List::Cons({
        head: 2,
        tail: List::Nil,
    }),
});
```

### Unit Variants as Syntactic Sugar

Unit variants are syntactic sugar for `Name(())` - a variant with unit type payload:

```wado
variant Maybe<T> {
    Some(T),
    None,        // internally None(())
}

// Construction - all equivalent
let n = Maybe::<i32>::None;
let n = Maybe::<i32>::None();
let n = Maybe::<i32>::None(());

// Access - .0 returns unit
let unit = n.0;  // ()

// Pattern matching - all equivalent
match maybe {
    Some(x) => ...,
    None => ...,      // preferred (short form)
}
match maybe {
    Some(x) => ...,
    None() => ...,    // also valid
}
match maybe {
    Some(x) => ...,
    None(()) => ...,  // also valid (explicit)
}
```

This ensures **all variant cases have a payload** (unit cases have `()` payload), making `.0` access uniformly valid.

Display/debug output uses the short form: `None` not `None(())`.

### Single-Element and Zero-Element Tuples

No special rules - these are allowed even if rarely useful:

```wado
variant Wrapper {
    Single([i32]),     // single-element tuple payload
    Empty([]),         // zero-element tuple payload
    Unit,              // unit payload = Unit(())
}

let s = Wrapper::Single([42]);
let val = s.0.0;  // 42

let e = Wrapper::Empty([]);
// e.0 is [] (empty tuple), NOT same as ()

let u = Wrapper::Unit;
// u.0 is () (unit value)
```

Note: `[]` (empty tuple) and `()` (unit) are distinct types.

### Pattern Matching Syntax

Both short and full paths work in patterns:

```wado
// Short form (within match on known variant type)
match shape {
    Circle(r) => ...,
    Rectangle([w, h]) => ...,
    Point => ...,
}

// Full path (always valid)
match shape {
    Shape::Circle(r) => ...,
    Shape::Rectangle([w, h]) => ...,
    Shape::Point => ...,
}

// if let requires full path for clarity
if let Shape::Circle(r) = shape { ... }
```

### Wildcard in Patterns

Wildcards work as expected:

```wado
if let Shape::Rectangle([w, _]) = shape {
    // Ignore height
    println(`width: ${w}`);
}
```

## Consequences

### Benefits

1. **No ambiguity**: `Foo(T)` is always scalar, `Foo([T, U])` is always tuple, `Foo({...})` is always struct
2. **Consistent syntax**: All non-unit payloads use `Name(payload)` form
3. **Consistent access**: Payload is always at `.0`
4. **Variant types**: Functions can accept specific variants, improving type safety
5. **Subset binding**: Union types enable flexible pattern matching against type subsets
6. **TypeScript familiarity**: Variant cases as types and union types align with TypeScript patterns
7. **Minimal special rules**: Edge cases (empty variants, single-element tuples) just work

### Trade-offs

1. **More verbose**: `r.0.0` instead of `r.0` for tuple element access
2. **Different from Rust**: Users from Rust may expect implicit tuple expansion and `Name { }` syntax
3. **Implementation complexity**: Variant cases as types and subset binding require additional type system work

### Comparison with Rust

| Aspect            | Rust                       | Wado                             |
| ----------------- | -------------------------- | -------------------------------- |
| Multiple payloads | `Foo(T, U)` implicit tuple | `Foo([T, U])` explicit           |
| Struct variants   | `Foo { a: T }`             | `Foo({ a: T })` with parens      |
| Payload access    | `.0`, `.1` directly        | `.0.0`, `.0.1` (payload at `.0`) |
| Variant as type   | Not supported              | `Shape::Circle` is a type        |
| Empty variants    | Unit/tuple/struct differ   | Unit only: `Foo`                 |
| Union types       | Not supported              | `A \| B` with subset binding     |

## Implementation Roadmap

### Phase 2: Pattern Matching

- [ ] Struct payload patterns (`if let Named({ width, height }) = shape`)
- [ ] `match` expressions and statements
- [ ] Generic variant pattern matching (`if let Some(x) = maybe` for `Maybe<T>`)
- [ ] `Result<T, E>` pattern matching (`if let Ok(v) = result`, `if let Err(e) = result`)

### Phase 3: Variant Cases as Types

- [ ] Variant case as standalone type (`Shape::Circle` as a type)
- [ ] Implicit coercion from variant case to variant type
- [ ] Field access on variant case types (`.0` for payload)
- [ ] Generic variant case types (`Option::<i32>::Some`)

### Phase 4: Union Types & Subset Binding

- [ ] Union type syntax (`A | B`)
- [ ] Union type normalization (flattening, deduplication to `Union<A>`)
- [ ] Discriminant generation for union types
- [ ] Subset binding in `if let` (`if let x: A | B = abc`)
- [ ] Subset binding in `match`
- [ ] Exhaustiveness checking for union types

### Completed

- [x] Variant construction (`Option::<T>::Some(x)`, `Color::Red`, `Shape::Circle(r)`)
- [x] Option pattern matching (`if let Some(x) = ...`)
- [x] Custom variant pattern matching (single-payload: `if let Circle(r) = shape`)
- [x] Value semantics (copy) for custom variants
- [x] Explicit tuple payload syntax `Name([T, U])` (Phase 1)
- [x] Reject implicit tuple expansion `Name(T, U)` (Phase 1)
- [x] Tuple payload patterns (`if let Rectangle([w, h]) = shape`) (Phase 1)
- [x] Unit payload patterns (`if let Point = shape`) (Phase 1)
- [x] `while let` / `for let` with custom variants (Phase 1)

## See Also

- [Variant Wasm GC Representation](./wep-2026-02-08-variant-representation.md) — how variants are laid out in Wasm GC (NullableRef vs SubtypeHierarchy)

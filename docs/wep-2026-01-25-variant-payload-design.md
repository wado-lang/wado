# WEP: Variant Payload Design

## Context

Rust's enum payload system has been criticized for being ad-hoc:

1. **Three variant forms**: unit, tuple, and struct variants with subtly different behaviors
2. **Implicit tuple expansion**: `Foo(a, b)` vs `Foo((a, b))` distinction is confusing
3. **Variants are not types**: Cannot write `fn process(c: Shape::Circle)` directly
4. **Inconsistencies**: Unit vs empty tuple vs empty struct have different initialization rules

Wado's variant system should learn from these issues and provide a cleaner, more consistent design.

### Current Wado Syntax

```wado
variant Shape {
    Circle(f64),           // single payload
    Rectangle(f64, f64),   // multiple payloads (implicit tuple)
    Point,                 // unit
}
```

The `Rectangle(f64, f64)` form implicitly creates a tuple, which mirrors Rust's problematic design.

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

Wado variants support exactly four payload forms:

| Form | Syntax | Example | Description |
|------|--------|---------|-------------|
| **Unit** | `Name` | `Point` | No payload |
| **Scalar** | `Name(T)` | `Circle(f64)` | Single value |
| **Tuple** | `Name([T, U, ...])` | `Rectangle([f64, f64])` | Explicit tuple type |
| **Struct** | `Name { field: T, ... }` | `Named { width: f64, height: f64 }` | Anonymous struct |

Key principle: **No implicit tuple expansion**. Multiple values require explicit tuple syntax `[T, U]` or struct syntax `{ a: T, b: U }`.

```wado
variant Shape {
    Point,                              // unit
    Circle(f64),                        // scalar
    Rectangle([f64, f64]),              // explicit tuple
    Named { width: f64, height: f64 },  // anonymous struct
}

// Construction
let p = Shape::Point;
let c = Shape::Circle(5.0);
let r = Shape::Rectangle([10.0, 20.0]);   // tuple literal required
let n = Shape::Named { width: 10.0, height: 20.0 };
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
    // c.0 is the radius (f64)
    println(`radius: {c.0}`);
}

// Can construct without full path when type is known
let circle: Shape::Circle = Shape::Circle(5.0);

// Subtype relationship: Shape::Circle <: Shape
let shape: Shape = circle;  // OK: upcast
```

This enables:
- Functions that accept only specific variants
- No need for `unreachable!()` in match arms
- Better type safety and documentation

### 3. Pattern Matching with Variant Types

```wado
fn area(s: Shape) -> f64 {
    match s {
        Shape::Circle(r) => 3.14159 * r * r,
        Shape::Rectangle([w, h]) => w * h,
        Shape::Named { width, height } => width * height,
        Shape::Point => 0.0,
    }
}

// When parameter type is Shape::Circle, no match needed
fn circle_area(c: Shape::Circle) -> f64 {
    return 3.14159 * c.0 * c.0;
}
```

### 4. Accessing Payload Fields

| Payload Type | Access Syntax | Example |
|--------------|---------------|---------|
| Scalar | `.0` | `circle.0` → radius |
| Tuple | `.0`, `.1`, etc. | `rect.0`, `rect.1` |
| Struct | `.field` | `named.width`, `named.height` |

```wado
let c = Shape::Circle(5.0);
let radius = c.0;           // 5.0

let r = Shape::Rectangle([10.0, 20.0]);
let width = r.0;            // 10.0
let height = r.1;           // 20.0

let n = Shape::Named { width: 10.0, height: 20.0 };
let w = n.width;            // 10.0
```

### 5. Generic Variants

```wado
variant Option<T> {
    Some(T),
    None,
}

// Option::Some<i32> is a type
let some: Option::Some<i32> = Option::Some(42);

// Subtype: Option::Some<i32> <: Option<i32>
let opt: Option<i32> = some;
```

### 6. Integration with Literal Types (Future)

When literal types are introduced, variant cases naturally integrate:

```wado
// Discriminant field with literal type
type Circle = Shape::Circle;  // { tag: "Circle", 0: f64 }

// Could potentially allow structural matching
type JsonValue =
    | { type: "null" }
    | { type: "bool", value: bool }
    | { type: "number", value: f64 }
    | { type: "string", value: String };
```

## Migration from Current Syntax

Current:
```wado
variant Shape {
    Rectangle(f64, f64),  // implicit tuple
}
let r = Shape::Rectangle(10.0, 20.0);
```

New:
```wado
variant Shape {
    Rectangle([f64, f64]),  // explicit tuple
}
let r = Shape::Rectangle([10.0, 20.0]);
```

The migration is mechanical: wrap multiple payload types in `[...]`.

## Consequences

### Benefits

1. **No ambiguity**: `Foo(T)` is always scalar, `Foo([T, U])` is always tuple
2. **Variant types**: Functions can accept specific variants, improving type safety
3. **Consistency**: Payload access follows struct/tuple conventions uniformly
4. **TypeScript familiarity**: Variant cases as types is similar to TypeScript's discriminated unions
5. **Future-proof**: Clean integration path with literal types

### Trade-offs

1. **More verbose**: `Rectangle([f64, f64])` instead of `Rectangle(f64, f64)`
2. **Learning curve**: Users from Rust may expect implicit tuple expansion
3. **Implementation complexity**: Variant cases as types require additional type system work

### Comparison with Rust

| Aspect | Rust | Wado |
|--------|------|------|
| Multiple payloads | `Foo(T, U)` implicit tuple | `Foo([T, U])` explicit |
| Struct variants | `Foo { a: T }` | `Foo { a: T }` (same) |
| Variant as type | Not supported | `Shape::Circle` is a type |
| Empty variants | Unit/tuple/struct behave differently | Unit only: `Foo` |

## Open Questions

1. **Syntax for variant type annotation**: `Shape::Circle` vs `Shape.Circle` vs other?
2. **Exhaustiveness**: How does the type checker verify all variants are handled?
3. **Coercion**: Should `Shape::Circle` auto-coerce to `Shape` in all contexts?
4. **Generic bounds**: Can we write `fn foo<T: Shape::Circle | Shape::Rectangle>(x: T)`?

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

Wado variants support exactly four payload forms with **consistent syntax**:

| Form | Syntax | Example | Description |
|------|--------|---------|-------------|
| **Unit** | `Name` | `Point` | No payload |
| **Scalar** | `Name(T)` | `Circle(f64)` | Single value |
| **Tuple** | `Name([T, U, ...])` | `Rectangle([f64, f64])` | Explicit tuple type |
| **Struct** | `Name({ field: T, ... })` | `Named({ width: f64, height: f64 })` | Anonymous struct in parens |

Key principles:
- **No implicit tuple expansion**: Multiple values require explicit tuple syntax `[T, U]` or struct syntax `{ a: T, b: U }`
- **Consistent wrapper**: All non-unit payloads use `Name(payload)` form

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
    println(`radius: {c.0}`);
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

// if let for single variant
if let Circle(r) = shape {
    println(`radius: {r}`);
}

// When parameter type is Shape::Circle, no match needed
fn circle_area(c: Shape::Circle) -> f64 {
    return 3.14159 * c.0 * c.0;
}
```

### 5. Accessing Payload Fields

| Payload Type | Access Syntax | Example |
|--------------|---------------|---------|
| Scalar | `.0` | `circle.0` → radius |
| Tuple | `.0`, `.1`, etc. | `rect.0`, `rect.1` |
| Struct | `.field` | `named.width`, `named.height` |

```wado
let c: Shape::Circle = Shape::Circle(5.0);
let radius = c.0;           // 5.0

let r: Shape::Rectangle = Shape::Rectangle([10.0, 20.0]);
let width = r.0;            // 10.0 (tuple .0)
let height = r.1;           // 20.0 (tuple .1)

let n: Shape::Named = Shape::Named({ width: 10.0, height: 20.0 });
let w = n.width;            // 10.0
```

## Union Type & Subset Binding

Beyond named variants, Wado supports anonymous union types with a powerful **subset binding** feature.

### Union Types

Union types are anonymous sum types that flatten automatically:

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
        println(`got A with a={a.a}`);
    }
}
```

This provides set-like operations that named variants cannot express:

| Feature | `variant` | Union Type |
|---------|-----------|------------|
| Named cases | ✓ | ✗ (structural) |
| Exhaustiveness checking | ✓ | ✓ |
| Subset binding | ✗ | ✓ |
| Set operations | ✗ | `\|` (union) |

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

## Migration from Current Syntax

Current:
```wado
variant Shape {
    Rectangle(f64, f64),        // implicit tuple
    Named { width: f64, height: f64 },  // struct without parens
}
let r = Shape::Rectangle(10.0, 20.0);
let n = Shape::Named { width: 10.0, height: 20.0 };
```

New:
```wado
variant Shape {
    Rectangle([f64, f64]),                   // explicit tuple
    Named({ width: f64, height: f64 }),      // struct with parens
}
let r = Shape::Rectangle([10.0, 20.0]);
let n = Shape::Named({ width: 10.0, height: 20.0 });
```

Migration steps:
1. Wrap multiple payload types in `[...]`
2. Wrap struct payloads in `({})`

## Consequences

### Benefits

1. **No ambiguity**: `Foo(T)` is always scalar, `Foo([T, U])` is always tuple, `Foo({...})` is always struct
2. **Consistent syntax**: All non-unit payloads use `Name(payload)` form
3. **Variant types**: Functions can accept specific variants, improving type safety
4. **Subset binding**: Union types enable flexible pattern matching against type subsets
5. **TypeScript familiarity**: Variant cases as types and union types align with TypeScript patterns
6. **Future-proof**: Clean integration path with literal types

### Trade-offs

1. **More verbose**: `Rectangle([f64, f64])` and `Named({...})` are longer
2. **Different from Rust**: Users from Rust may expect implicit tuple expansion and `Name { }` syntax
3. **Implementation complexity**: Variant cases as types and subset binding require additional type system work

### Comparison with Rust

| Aspect | Rust | Wado |
|--------|------|------|
| Multiple payloads | `Foo(T, U)` implicit tuple | `Foo([T, U])` explicit |
| Struct variants | `Foo { a: T }` | `Foo({ a: T })` with parens |
| Variant as type | Not supported | `Shape::Circle` is a type |
| Empty variants | Unit/tuple/struct differ | Unit only: `Foo` |
| Union types | Not supported | `A \| B` with subset binding |

## Implementation Roadmap

Current status: variant construction, single-payload if-let pattern matching, and value semantics work for the old syntax.

### Phase 1: Syntax Update

- [ ] Update parser for explicit tuple payload `Name([T, U])`
- [ ] Update parser for struct payload with parens `Name({ field: T })`
- [ ] Reject implicit tuple expansion `Name(T, U)`
- [ ] Update existing tests and fixtures

### Phase 2: Pattern Matching

- [ ] Tuple payload patterns (`if let Rectangle([w, h]) = shape`)
- [ ] Struct payload patterns (`if let Named({ width, height }) = shape`)
- [ ] Unit payload patterns with else (`if let Point = shape { ... } else { ... }`)
- [ ] `while let` / `for let` with custom variants
- [ ] `match` expressions and statements
- [ ] Generic variant pattern matching (`if let Some(x) = maybe` for `Maybe<T>`)
- [ ] `Result<T, E>` pattern matching (`if let Ok(v) = result`, `if let Err(e) = result`)

### Phase 3: Variant Cases as Types

- [ ] Variant case as standalone type (`Shape::Circle` as a type)
- [ ] Implicit coercion from variant case to variant type
- [ ] Field access on variant case types (`.0`, `.field`)
- [ ] Generic variant case types (`Option::<i32>::Some`)

### Phase 4: Union Types & Subset Binding

- [ ] Union type syntax (`A | B`)
- [ ] Union type flattening (`(A | B) | C` → `A | B | C`)
- [ ] Discriminant generation for union types
- [ ] Subset binding in `if let` (`if let x: A | B = abc`)
- [ ] Exhaustiveness checking for union types

### Completed

- [x] Variant construction (`Option::<T>::Some(x)`, `Color::Red`, `Shape::Circle(r)`)
- [x] Option pattern matching (`if let Some(x) = ...`)
- [x] Custom variant pattern matching (single-payload: `if let Circle(r) = shape`)
- [x] Value semantics (copy) for custom variants

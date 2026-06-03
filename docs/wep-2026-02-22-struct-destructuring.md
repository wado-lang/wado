# WEP: Struct Destructuring

## Context

Wado supports tuple destructuring (`let [a, b] = pair;`) and variant pattern matching (`if let Some(x) = opt`), but has no way to destructure structs in patterns. Users must access fields individually:

```wado
// Current workaround
let x = point.x;
let y = point.y;
```

This is verbose and doesn't compose well with nested patterns or variant struct payloads.

### Design Goals

1. **Rust-based with JS/TS ergonomics**: Follow Rust's struct pattern syntax, but allow omitting the type name like JS/TS
2. **Symmetry with construction**: Destructuring mirrors struct literal syntax
3. **Composability**: Works in all pattern contexts (let, if let, match, for-of, matches)
4. **Consistency**: Integrates with existing tuple and variant patterns

### Rust vs JS/TS Comparison

| Feature        | Rust                                | JS/TS                       | Wado (this WEP)            |
| -------------- | ----------------------------------- | --------------------------- | -------------------------- |
| Type name      | Required: `let Point { x, y } = p;` | None: `const { x, y } = p;` | Optional: both work        |
| Renaming       | `field: binding`                    | `field: binding`            | `field: binding`           |
| Rest           | `..` (ignore)                       | `...rest` (collect)         | `..` (ignore)              |
| Nested         | Via rename: `{ a: { b } }`          | Via rename: `{ a: { b } }`  | Via rename: `{ a: { b } }` |
| Default values | No                                  | `{ x = 0 }`                 | No                         |

## Decision

### 1. Basic Struct Destructuring

Both named (Rust-style) and unnamed (JS-style) forms are supported:

```wado
struct Point { x: i32, y: i32 }
let p = Point { x: 10, y: 20 };

// Named — explicit type, self-documenting
let Point { x, y } = p;

// Unnamed — concise, type inferred from RHS
let { x, y } = p;
```

The shorthand `{ x, y }` binds variables with the same name as the fields, mirroring the shorthand construction `Point { x, y }`.

### 2. Field Renaming

Use `field: binding` syntax to bind a field to a different variable name:

```wado
let { x: horizontal, y: vertical } = point;
// horizontal == 10, vertical == 20
```

This mirrors the construction duality:

```wado
// Construction:     field: value      → puts value into field
// Destructuring:    field: binding    → extracts field into binding
let p = Point { x: horizontal, y: vertical };   // construction
let { x: horizontal, y: vertical } = p;          // destructuring
```

Shorthand and renamed fields can be mixed:

```wado
let { x, y: vertical } = point;
// x == 10, vertical == 20
```

### 3. Rest Pattern (`..`)

Use `..` to ignore unmentioned fields. Without `..`, all fields must be listed (exhaustiveness):

```wado
struct Person { name: String, age: i32, email: String }

// All fields required (exhaustive)
let { name, age, email } = person;

// Ignore some fields with ..
let { name, .. } = person;

// With renaming
let { name: n, .. } = person;
```

`..` must appear at the end of the field list. It does not collect remaining fields into a variable — it only ignores them.

```wado
// ERROR: .. must be last
let { .., name } = person;

// ERROR: cannot bind rest (use field access instead)
let { name, ..rest } = person;
```

### 4. Mutable Bindings

`let mut` makes all bound variables mutable, consistent with tuple destructuring:

```wado
let mut { x, y } = point;
x += 1;  // OK: x is mutable
y += 1;  // OK: y is mutable
```

### 5. Nested Destructuring

Field renaming naturally extends to nested patterns. The binding position accepts any pattern:

```wado
struct Line { start: Point, end: Point }

// Nested struct destructuring
let { start: { x: x1, y: y1 }, end: { x: x2, y: y2 } } = line;

// Mixed with rest
let { start: { x, .. }, .. } = line;

// Nested with tuple
struct Tagged { label: String, coords: [i32, i32] }
let { label, coords: [cx, cy] } = tagged;
```

### 6. Pattern Contexts

Struct patterns work in all existing pattern contexts:

#### Let Binding

```wado
let { x, y } = point;
let Point { x, y } = point;
```

#### If Let (Variant Struct Payloads)

```wado
variant Shape {
    Circle(f64),
    Named({ width: f64, height: f64 }),
    Point,
}

if let Named({ width, height }) = shape {
    println(`area: {width * height}`);
}
```

#### Match

```wado
match shape {
    Circle(r) => 3.14159 * r * r,
    Named({ width, height }) => width * height,
    Point => 0.0,
}

// With guards
match person {
    { name, age } && age >= 18 => `{name} is an adult`,
    { name, .. } => `{name} is a minor`,
}
```

#### For-Of

```wado
let people: List<Person> = [...];
for let { name, age } of people {
    println(`{name}: {age}`);
}
```

#### Matches Operator

```wado
if point matches { Point { x, y } && x > 0 } {
    println("positive x");
}

// Unnamed form
if person matches { { age, .. } && age >= 18 } {
    println("adult");
}
```

### 7. Named Form Type Checking

When the type name is specified, the compiler verifies the expression type matches:

```wado
struct Point { x: i32, y: i32 }
struct Vec2 { x: i32, y: i32 }

let p = Point { x: 1, y: 2 };

let Point { x, y } = p;   // OK: p is Point
// let Vec2 { x, y } = p;  // ERROR: expected Vec2, found Point
```

The unnamed form `{ x, y }` infers the struct type from the expression and does not require a type match.

### 8. Literal Patterns in Fields

Field values can be matched against literals (integers, floats, booleans, characters, strings) in refutable pattern contexts (match, if let, matches):

```wado
struct Config { name: String, value: i32 }

// Match with literal in field position
match config {
    Config { name, value: 0 } => `{name} is zero`,
    Config { name, value: 42 } => `{name} is the answer`,
    Config { name, value } => `{name} = {value}`,
}

// If let with literal field
if let Config { name, value: 0 } = config {
    println(`{name} is zero`);
}

// Matches with literal field
if config matches { Config { value: 0, .. } } {
    println("zero config");
}
```

Literal patterns in fields are refutable — they can fail to match. They are only allowed in refutable contexts (match, if let, matches), not in irrefutable let bindings.

### 9. Wildcard Fields

Individual fields can be ignored with `_`:

```wado
let { x, y: _ } = point;   // bind x, ignore y
let { x: _, y } = point;   // ignore x, bind y
```

This is different from `..` which ignores all unmentioned fields. With `_`, the field must still be explicitly listed.

## Consequences

### Benefits

1. **Ergonomic**: Eliminates verbose field-by-field access
2. **Composable**: Works with variant payloads, nesting, and all pattern contexts
3. **Familiar**: Follows established conventions from Rust and JS/TS
4. **Symmetric**: Destructuring mirrors construction syntax

### Trade-offs

1. **Two forms**: Both `{ x, y }` and `Point { x, y }` are valid, which is flexible but adds parser complexity
2. **No rest collection**: `..rest` is not supported; users must access remaining fields individually if needed
3. **No default values**: Unlike JS/TS, no `{ x = 0 }` syntax for defaults

### Not Included (Possible Future Extensions)

- **Rest collection** (`..rest`): Collecting remaining fields into a new struct. Complex to implement and type, low priority.
- **Default values** (`{ x = 0 }`): JS/TS feature for providing defaults. Adds complexity and overlaps with `Option` patterns.
- **Function parameter destructuring**: `fn f({ x, y }: Point)` — useful but a separate feature.
- **Struct update syntax**: `Point { x: 1, ..base }` — construction-side feature, separate WEP.

## See Also

- [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md) — struct payloads in variants use `Named({ field: T })` syntax
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md) — struct definition and construction
- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) — tuple destructuring patterns

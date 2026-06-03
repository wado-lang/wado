# WEP: Tuple Destructuring

## Context

Wado uses `[...]` syntax for both tuple literals and tuple types (see [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)). Tuple destructuring allows extracting individual elements from a tuple value into separate bindings using a mirrored `[...]` pattern.

This feature is the tuple counterpart to [Struct Destructuring](./wep-2026-02-22-struct-destructuring.md). While struct patterns use field names, tuple patterns use positional correspondence.

### Design Goals

1. **Symmetry with construction**: `let [a, b] = [1, "hello"]` mirrors `[1, "hello"]`
2. **Composability**: Works in all pattern contexts (let, if let, match, for-of, matches)
3. **Consistency with struct destructuring**: Wildcard `_` and rest `..` behave analogously
4. **Element count safety**: Mismatched element counts are compile-time errors

## Decision

### 1. Basic Tuple Destructuring

Bind each element of a tuple to a variable by position:

```wado
fn make_pair() -> [i32, String] {
    return [42, "hello"];
}

let [a, b] = make_pair();
// a == 42, b == "hello"
```

The number of patterns must match the tuple's element count exactly (unless `..` is used).

### 2. Wildcard Elements (`_`)

Use `_` to ignore individual elements at specific positions:

```wado
let [first, _, third] = [1, 2, 3];
// first == 1, third == 3 (second element ignored)

let [_, second, _] = [1, 2, 3];
// second == 2
```

Each `_` consumes exactly one element at its position.

### 3. Rest Pattern (`..`)

Use `..` to ignore all remaining elements. `..` must appear at the end of the pattern:

```wado
let [first, ..] = [1, 2, 3, 4, 5];
// first == 1 (remaining 4 elements ignored)

let [a, b, ..] = [1, 2, 3];
// a == 1, b == 2
```

Unlike `_` which ignores exactly one element, `..` ignores zero or more trailing elements.

```wado
// ERROR: .. must be at the end
let [.., last] = tuple;

// ERROR: only one .. allowed
let [first, .., .., last] = tuple;
```

### 4. Mutable Bindings

`let mut` makes all bound variables mutable, consistent with struct destructuring:

```wado
let mut [a, b] = [10, 20];
a += 1;  // OK: a is mutable
b += 1;  // OK: b is mutable
```

### 5. Nested Destructuring

Tuple patterns compose recursively:

```wado
// Nested tuples
let [[a, b], c] = [[1, 2], "nested"];
// a == 1, b == 2, c == "nested"

// Tuple inside struct
struct Tagged { label: String, coords: [i32, i32] }
let { label, coords: [cx, cy] } = tagged;

// Struct inside tuple
let [{ x, y }, name] = [point, "origin"];
```

### 6. Element Count Mismatch

When the number of pattern elements doesn't match the tuple type (and no `..` is used), the compiler emits an error:

```wado
fn make_pair() -> [i32, i32] {
    return [1, 2];
}

let [a, b, c] = make_pair();
// ERROR: tuple destructuring pattern with 3 elements,
//        but the tuple has 2 elements
```

### 7. Pattern Contexts

Tuple patterns work in all existing pattern contexts:

#### Let Binding

```wado
let [a, b] = pair;
let [first, ..] = triple;
```

#### If Let

```wado
let opt: Option<[i32, i32]> = Option::<[i32, i32]>::Some([10, 20]);
if let Some([x, y]) = opt {
    println(`{x}, {y}`);
}
```

#### Match

```wado
// Direct tuple matching
let pair = [1, 2];
let desc = match pair {
    [0, 0] => "origin",
    [x, y] && x == y => "diagonal",
    [x, y] => `({x}, {y})`,
};

// Variant with tuple payload
variant Shape {
    Rectangle([f64, f64]),
    Circle(f64),
    Point,
}

let area = match shape {
    Rectangle([w, h]) => w * h,
    Circle(r) => 3.14159 * r * r,
    Point => 0.0,
};
```

#### For-Of

```wado
let pairs: List<[i32, String]> = [[1, "one"], [2, "two"]];
for let [num, name] of pairs {
    println(`{num}: {name}`);
}
```

#### Matches Operator

```wado
let pair = [10, 20];
if pair matches { [x, y] && x + y > 25 } {
    println("large pair");
}
```

## Consequences

### Benefits

1. **Intuitive**: Pattern mirrors the construction syntax `[a, b]`
2. **Positional**: No field names needed — order determines binding
3. **Safe**: Element count mismatches caught at compile time
4. **Composable**: Nests freely with struct patterns and variant patterns

### Trade-offs

1. **Positional only**: Unlike struct patterns, meaning depends on position rather than names. For large tuples, consider using a struct instead.
2. **No middle rest**: `..` only works at the end — extracting the last element of an arbitrary-length tuple is not supported.
3. **No named tuple elements**: Tuple elements don't have names; use struct destructuring with renaming if names are desired.

### Not Included (Possible Future Extensions)

- **Middle/leading rest** (`let [first, .., last] = tuple`): Requires compile-time length arithmetic. Low priority since large tuples should use structs.
- **Slice patterns**: Destructuring `List<T>` with patterns like `[first, ...rest]` where `rest` is a sub-array. Separate feature from tuple destructuring.

## See Also

- [Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md) — tuple syntax design
- [Struct Destructuring](./wep-2026-02-22-struct-destructuring.md) — the struct counterpart
- [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md) — tuple payloads in variants

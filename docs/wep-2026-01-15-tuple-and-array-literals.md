# WEP: Tuple and List Literal Syntax

## Context

Wado needs to define syntax for tuple and array literals. The original spec used:

- `[...]` for array literals: `[1, 2, 3]` → `List<i32>`
- `(...)` for tuple literals: `(1, "hello")` → `Tuple<i32, String>`

This follows Rust's conventions. However, several factors suggest reconsidering this design.

### TypeScript Conventions

TypeScript, the most likely source of Wado's target audience, uses `[...]` for both tuples and arrays:

```typescript
// TypeScript
let arr: number[] = [1, 2, 3]; // List
let tuple: [number, string] = [1, "hi"]; // Tuple
```

TypeScript developers expect `[...]` syntax for tuples.

### JSON Interoperability

Wado supports importing JSON modules:

```wado
use config from "./config.json" with { type: "json" };
```

JSON arrays use `[...]` syntax and are inherently heterogeneous:

```json
{
  "point": [10, 20],
  "mixed": [1, "hello", true, null]
}
```

Since Wado's `List<T>` is homogeneous (all elements same type), JSON's heterogeneous arrays naturally map to tuples. Having `[...]` represent tuples makes this mapping intuitive—the syntax visually matches between JSON and Wado.

### Unit Type Consideration

The question arose: if `(...)` is no longer used for tuples, what represents the unit type?

Analysis of unit type use cases in Rust:

1. Functions with no meaningful return value
2. `Result<(), E>` for fallible operations with no success payload
3. Generic type parameter placeholder
4. Callbacks returning nothing

The most common case is `Result<(), E>`. Alternatives like `Result<[], E>` or `Result<void, E>` are visually noisy or semantically unclear.

## Decision

### 1. `[...]` is a Tuple Literal by Default

```wado
let t = [1, 2, 3];           // [i32, i32, i32] (tuple)
let pair = [1, "hello"];     // [i32, String] (tuple)
```

### 2. Tuple Types Use Bracket Syntax

```wado
// Old syntax
let x: Tuple<i32, String> = ...;

// New syntax
let x: [i32, String] = ...;
```

### 3. Arrays Require Explicit Conversion

```wado
let a = [1, 2, 3] as List<i32>;  // Explicit conversion
```

### 4. Implicit Coercion at Compile-Time

When the target type is known at compile time, implicit coercion is allowed:

```wado
fn takes_array(a: List<i32>) { ... }

takes_array([1, 2, 3]);  // OK - compiler knows target type
```

Runtime conversions require explicit `as`:

```wado
let x = [1, 2, 3];               // Tuple (no context)
let y = [1, 2, 3] as List<i32>; // Explicit conversion
```

### 5. Keep `()` for Unit Type

The unit type and value remain `()`:

```wado
fn run() -> Result<(), Error> {
    Ok(())
}
```

### 6. Empty `[]` is Empty Tuple

`[]` represents an empty tuple, distinct from `()` (unit):

```wado
let empty_tuple: [] = [];
let unit: () = ();
// These are distinct types
```

While rarely used, this maintains consistency.

### 7. Single Element `[x]` is a 1-Tuple

```wado
let single: [i32] = [42];  // 1-tuple, distinct from bare i32
let bare: i32 = 42;
```

### 8. Trailing Comma is Allowed

```wado
let t = [1, 2, 3,];  // OK
```

## Summary

| Syntax                   | Type              | Notes                       |
| ------------------------ | ----------------- | --------------------------- |
| `()`                     | `()`              | Unit type/value (unchanged) |
| `[]`                     | `[]`              | Empty tuple                 |
| `[42]`                   | `[i32]`           | Single-element tuple        |
| `[1, 2, 3]`              | `[i32, i32, i32]` | Tuple                       |
| `[1, "a"]`               | `[i32, String]`   | Heterogeneous tuple         |
| `[1, 2, 3,]`             | `[i32, i32, i32]` | Trailing comma OK           |
| `[1, 2, 3] as List<i32>` | `List<i32>`       | Explicit array              |

| Type Syntax          | Notes                     |
| -------------------- | ------------------------- |
| `[i32, String]`      | Tuple type (preferred)    |
| `Tuple<i32, String>` | Alias for `[i32, String]` |
| `List<i32>`          | List type (unchanged)     |
| `()`                 | Unit type (unchanged)     |

## Consequences

### Positive

1. **TypeScript familiarity**: Aligns with TypeScript's tuple syntax, reducing learning curve for the target audience
2. **JSON interoperability**: `[...]` syntax visually matches JSON arrays, making the mapping intuitive
3. **Concise type syntax**: `[i32, String]` is shorter than `Tuple<i32, String>`
4. **Consistent with `as` operator**: Uses existing `as` for type conversion rather than introducing new syntax
5. **Clear coercion rules**: Compile-time implicit, runtime explicit

### Negative

1. **Diverges from Rust**: Rust developers expect `()` for tuples and `[]` for arrays
   - **Mitigation**: TypeScript audience is primary target; clear documentation
2. **Potential confusion**: `[1, 2, 3]` being a tuple (not array) may surprise some
   - **Mitigation**: Consistent with TypeScript; explicit `as List<T>` makes intent clear
3. **Two "empty" concepts**: Both `[]` (empty tuple) and `()` (unit) exist
   - **Mitigation**: They serve different purposes; `()` for unit semantics, `[]` for tuple consistency

## References

- [TypeScript Tuple Types](https://www.typescriptlang.org/docs/handbook/2/objects.html#tuple-types)
- [JSON Specification](https://www.json.org/)
- Current Wado spec: `spec.md` (List Literals, Tuple Literals sections)

# WEP: Literal Type Conversion Rules

**Date**: 2026-01-12
**Status**: Accepted

## Context

Wado needs clear rules for implicit type conversions involving numeric literals. The question is: when should the compiler allow implicit conversions, and when should it require explicit `as` casts?

The core cases to consider:

```wado
let x: i64 = 32;           // Literal to variable
let y = 32 as i64;         // Explicit cast (always allowed)
let z: i32 = x;            // Variable to different-typed variable
```

We investigated how modern statically-typed languages (Rust, Go, Swift, Zig) handle literal type inference and implicit conversions.

### Language Survey

| Language  | Approach                    | Key Features                                                                                                                       |
| --------- | --------------------------- | ---------------------------------------------------------------------------------------------------------------------------------- |
| **Rust**  | Minimal implicit conversion | Default types (`i32`, `f64`), explicit `as` required for most conversions                                                          |
| **Go**    | Untyped constants           | [Untyped constants](https://go.dev/blog/constants) with arbitrary precision, flexible until typed, strict afterward                |
| **Swift** | Protocol-based literals     | [Default types](https://developer.apple.com/documentation/swift/default-literal-types) (`Int`, `Double`), context-driven inference |
| **Zig**   | Compile-time types          | [`comptime_int`/`comptime_float`](https://zig.guide/language-basics/comptime/) coerce to any compatible type, strict at runtime    |

### Conversion Patterns

| Case                         | Example                            | Rust | Go  | Zig | Swift |
| ---------------------------- | ---------------------------------- | ---- | --- | --- | ----- |
| Literal → variable           | `let x: i64 = 32;`                 | ✅   | ✅  | ✅  | ✅    |
| Untyped literal → variable   | Go: `const C = 32; var x i64 = C`  | N/A  | ✅  | ✅  | ✅    |
| Typed value → different type | `let x: i64 = 32; let z: i32 = x;` | ❌   | ❌  | ❌  | ❌    |

**Key finding**: All languages allow flexibility at the "untyped" stage only. Once a value has a concrete type, implicit conversions are disallowed.

### Three Design Options

**Option A: Minimal (Rust-style)**

- ✅ Literals can be implicitly converted to any compatible type
- ❌ Variables require explicit `as` casts for type conversion
- Simple, predictable, separates type checking from optimization

**Option B: Untyped Literals (Go/Zig-style)**

- Variables without type annotation remain "untyped" until used
- Untyped values can convert to any compatible type
- Once typed (via annotation or inference), conversions require `as`
- More complex, introduces "untyped" concept

**Option C: Constant Propagation-based**

- Allow conversions when compiler knows the value at compile time
- Most flexible but couples type checking with optimization
- Unclear boundaries for when propagation applies
- Contradicts explicitness philosophy

## Decision

**Adopt Option A: Minimal implicit conversion (Rust-style)**

### Rules

1. **Literals allow implicit conversion**:

   ```wado
   let x: i64 = 32;        // ✅ Literal → any compatible type
   let y: i32 = 42;        // ✅
   foo(100);               // ✅ Function argument
   ```

2. **Variables require explicit conversion**:

   ```wado
   let x: i64 = 32;
   let y: i32 = x;         // ❌ Error: type mismatch
   let z: i32 = x as i32;  // ✅ Explicit cast
   ```

3. **Range checking for literals**:

   ```wado
   let overflow: i8 = 128;  // ❌ Compile error: out of range
   ```

4. **No literal type suffixes**:
   - Suffixes like `32i64` or `3.14f32` are **not supported**
   - Use explicit casts or type annotations instead
   - Rationale: Suffixes are not self-evident and add syntax complexity

## Consequences

### Benefits

1. **Aligns with Wado's philosophy**:
   - Explicit imports → Explicit conversions
   - Zero abstraction to Wasm → Conversion costs are visible
   - Type system maps directly to Component Model

2. **Simple and predictable**:
   - Easy to learn: "literals are flexible, variables are strict"
   - No ambiguity about when conversions happen
   - Clear error messages

3. **Clean implementation**:
   - Type checking and optimization remain separate
   - No need for "untyped" type category
   - No constant propagation in type checker

4. **Prevents subtle bugs**:
   - Narrowing conversions (`i64` → `i32`) are always visible
   - Developer must acknowledge potential data loss with `as`

### Trade-offs

1. **More `as` keywords**:
   - Variable-to-variable conversions require explicit casts
   - Slightly more verbose than Go/Zig in some cases
   - However, this verbosity serves explicitness and safety

2. **No optimization-dependent typing**:
   - Cannot leverage constant propagation for implicit conversions
   - Accepted trade-off for simplicity and predictability

### Examples

```wado
// ✅ Good: Literals are flexible
fn calculate(a: i32, b: i64) -> f64 {
    let sum = (a as i64) + b;
    return sum as f64;
}

calculate(10, 20);  // Literals infer correct types

// ✅ Good: Explicit conversions
let x: i64 = 100;
let y: i32 = x as i32;  // Intent is clear

// ✅ Good: Type annotations guide inference
let coords: [f64, f64] = [0, 0];  // Integers → f64

// ❌ Error: Requires explicit cast
let a: i64 = 42;
let b: i32 = a;  // Error: cannot assign i64 to i32

// ❌ Error: Literal out of range
let byte: i8 = 200;  // Error: value 200 exceeds i8 range (-128..127)
```

## References

- [Constants - The Go Programming Language](https://go.dev/blog/constants)
- [Type Coercions - Rust RFC](https://rust-lang.github.io/rfcs/0401-coercions.html)
- [Comptime - zig.guide](https://zig.guide/language-basics/comptime/)
- [Default Literal Types - Apple Developer](https://developer.apple.com/documentation/swift/default-literal-types)
- [Constant Propagation - GeeksforGeeks](https://www.geeksforgeeks.org/constant-propagation-in-complier-design/)

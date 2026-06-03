# Tagged Template Literals for Compile-Time Execution

## Context

Wado programs frequently need to embed binary data (images, cryptographic keys, compiled assets) or validate complex string literals (regular expressions, SQL queries, DSL syntax) at compile time. Several challenges arise:

1. **Binary data embedding**: Developers need to embed base64-encoded or hex-encoded data as `List<u8>` literals without runtime decoding overhead
2. **DSL validation**: String literals representing domain-specific languages (regex, SQL, GraphQL) should be validated at compile time to catch errors early
3. **Zero runtime cost**: These operations should happen at compile time with no runtime overhead
4. **Extensibility**: Users should be able to define their own compile-time transformations without language modifications

### Alternatives Considered

**Option 1: Built-in literal prefixes** (like Rust/Python)

```wado
let data = base64"SGVsbG8gV29ybGQ=";
let hex_data = hex"48656c6c6f";
```

- ✅ Familiar to Rust/Python developers
- ✅ Clear that it's a built-in feature
- ❌ Not extensible - requires language changes for new encodings
- ❌ Violates Wado's "minimal built-ins" philosophy
- ❌ Cannot support user-defined DSLs

**Option 2: Runtime library functions**

```wado
let data = base64_decode("SGVsbG8gV29ybGQ=");
```

- ✅ Simple and explicit
- ❌ Runtime overhead for decoding
- ❌ Errors only caught at runtime
- ❌ Cannot optimize away the decoding logic

**Option 3: Tagged template literals** (like JavaScript, with compile-time execution)

```wado
let data = base64`SGVsbG8gV29ybGQ=`;
```

- ✅ Extensible by users
- ✅ Compile-time execution with zero runtime cost
- ✅ Consistent with Wado's "pure functions are compile-time executable" model
- ✅ Supports arbitrary return types
- ⚠️ Requires compile-time evaluation engine
- ⚠️ More complex implementation

## Decision

We adopt **tagged template literals** with compile-time function execution, inspired by JavaScript's tagged templates and Zig's `comptime`.

### Syntax

```wado
let result = tag`literal string`;
```

Where `tag` is an effect-free function with signature:

```wado
fn tag(input: String) -> T {
    // Pure function body
}
```

### Semantics

1. **Compile-time execution**: The tagged function is executed during compilation
2. **Effect-free requirement**: Only functions with no `with` clause can be used as tags
3. **Pure computation**: The function can only call other effect-free functions
4. **Error handling**: If the function panics, it becomes a compile error
5. **Type flexibility**: The function can return any type `T`

### Standard Library Examples

```wado
// In core:encoding
pub fn base64(input: String) -> List<u8> {
    match decode_base64_impl(input) {
        Ok(data) => data,
        Err(e) => panic(`Invalid base64 encoding: {e}`),
    }
}

pub fn hex(input: String) -> List<u8> {
    match decode_hex_impl(input) {
        Ok(data) => data,
        Err(e) => panic(`Invalid hex encoding: {e}`),
    }
}
```

### User-Defined Examples

```wado
// Compile-time regex validation
fn regex(pattern: String) -> Regex {
    match compile_regex(pattern) {
        Ok(r) => r,
        Err(e) => panic(`Invalid regex pattern: {e}`),
    }
}

let email_pattern = regex`^[a-z]+@[a-z]+\.[a-z]+$`;  // Validated at compile time

// SQL query validation
fn sql(query: String) -> SqlQuery {
    match parse_sql(query) {
        Ok(q) => q,
        Err(e) => panic(`Invalid SQL syntax: {e}`),
    }
}

let query = sql`SELECT * FROM users WHERE id = ?`;  // Validated at compile time
```

## Consequences

### Positive

1. **Zero runtime overhead**: Decoding/validation happens at compile time, binary contains pre-processed results
2. **Early error detection**: Invalid literals cause compile errors with helpful messages
3. **Extensibility**: Users can define custom compile-time transformations without language changes
4. **Type safety**: Return type is checked statically
5. **Consistency**: Aligns with Wado's "pure functions" model - effect-free functions are inherently compile-time executable
6. **No special cases**: `base64`, `hex`, etc. are just standard library functions, not language keywords

### Negative

1. **Implementation complexity**: Requires a compile-time evaluation engine for Wasm
2. **Compilation time**: Complex compile-time computations can slow down builds
3. **Debugging**: Compile-time errors may be harder to debug than runtime errors
4. **Determinism requirements**: Must ensure compile-time execution is deterministic (already addressed by Wado's deterministic libm)

### Trade-offs

- **No I/O at compile time**: Functions with effects cannot be called during compile-time execution (enforced by effect system)
- **Recursion depth limits**: Compile-time recursion must have reasonable depth limits to prevent infinite loops
- **Heap allocation**: Allowed via Wasm GC, unlike Rust's const fn

## Implementation Phases

### Phase 1: Simple string literals (MVP)

- Support syntax: `tag`literal``
- Function signature: `fn(String) -> T`
- No interpolation support
- Standard library: `base64`, `hex`

### Phase 2: Interpolation support (Future)

- Support syntax: `` tag`text {expr} more` ``
- Function signature: `fn(CookedStrings/RawStrings, Values) -> T` where `Values` is a tuple type
- See [String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md) for the full interpolation design
- See [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md) for iterating over heterogeneous values

### Phase 3: Compile-time I/O (Future consideration)

- Carefully designed `embed` function for file inclusion
- Security implications need thorough analysis
- May require sandboxing or explicit opt-in

## Related WEPs

- [String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md): Defines how tagged templates with interpolation are desugared
- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md): Required for iterating over heterogeneous interpolated values in tag functions

## References

- **Zig's comptime**: [What is Zig's Comptime?](https://kristoff.it/blog/what-is-zig-comptime/)
- **Rust's const fn**: [Constant evaluation - The Rust Reference](https://doc.rust-lang.org/reference/const_eval.html)
- **JavaScript tagged templates**: [Template literals (MDN)](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
- **Rust binary_macros crate**: Similar compile-time binary decoding

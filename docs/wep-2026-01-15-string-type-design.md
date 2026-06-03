# WEP: String Type Design

## Status

Accepted

## Context

Wado requires a string type design that balances several competing concerns:

### Requirements

1. **Efficient concatenation**: String concatenation must be efficient enough for typical use cases (loops, builders, etc.)
2. **Value semantics**: Aligned with Wado's design philosophy for struct types
3. **No separate StringBuilder**: Users shouldn't need a separate builder type for efficient string construction
4. **Unicode correctness**: Proper handling of UTF-8 encoded Unicode
5. **Component Model alignment**: Direct mapping to Component Model's `string` type (UTF-8)
6. **GC compatibility**: Work well with Wasm GC

### Design Challenges

Different languages have taken different approaches:

- **Rust**: Two types (`String`/`str`), value semantics, complex but explicit
- **Go**: Single type, byte array semantics, simple but requires `strings.Builder` for efficiency
- **TypeScript/JavaScript**: Immutable strings, reference semantics, requires concatenation patterns for efficiency
- **Python/Swift**: Copy-on-Write strings, efficient but requires runtime reference counting

For Wado, key tensions include:

- Value semantics vs efficient mutation
- Single type vs multiple types for different use cases
- Explicitness vs hidden optimizations
- Wasm GC constraints (no native reference count inspection)

## Decision

We adopt a **single String type with value semantics and specialized `+=` operator**:

### Core Design

```wado
// String: value semantics, immutable content, GC-managed
struct String {
    data: GcArray<u8>,   // UTF-8 data
    len: usize,
    capacity: usize,     // Buffer for += operations
}
```

### Key Features

**1. Index access is prohibited**

```wado
// Prohibited
s[i]      // Compile error
s[i..j]   // Compile error

// Explicit methods instead
s.bytes() -> List<u8>      // Get as byte array
s.chars() -> List<char>    // Get as character array
s.len() -> usize
s.is_empty() -> bool
```

**Rationale**:

- Avoids ambiguity between byte indexing and character indexing
- Follows Rust's precedent
- Forces explicit choice between byte/character semantics

**2. Value semantics with reference sharing**

```wado
let s1 = "hello";
let s2 = s1;  // Semantics: value copy, Implementation: reference sharing (safe because immutable)
```

**3. Specialized `+=` operator**

```wado
let mut s = "hello";
s += " world";  // Desugars to: String::add_assign(&mut s, " world")
                // Efficient implementation with capacity management
```

**Implementation**:

```wado
fn add_assign(target: &mut String, suffix: String) {
    if target.capacity >= target.len + suffix.len {
        // In-place append (no allocation)
    } else {
        // Reallocation with growth factor (typically 2x)
    }
}
// Amortized O(1) per operation in loops
```

**4. Regular `+` creates new String**

```wado
let s = "hello" + " world";  // Creates new String
```

**5. Pre-allocation support**

```wado
let mut s = String::with_capacity(1000);
for item in items {
    s += item;  // Efficient, no reallocations if within capacity
}
```

### Consistency Considerations

The `+=` operator is treated specially for String:

```wado
// For numeric types
x += 5  ≡  x = x + 5  // Syntactic sugar

// For String
s += t  ≠  s = s + t  // Different implementation (but same semantic result)
```

**Mitigation**: This will be generalized through traits in the future:

```wado
trait AddAssign<Rhs> {
    fn add_assign(&mut self, rhs: Rhs);
}

// All += operations desugar to:
x += y  →  AddAssign::add_assign(&mut x, y)
```

This makes the special treatment a general mechanism available to user types.

## Consequences

### Positive

1. **Simple mental model**: Single String type, value semantics
2. **Efficient concatenation**: `+=` provides O(n) amortized complexity in loops
3. **No StringBuilder needed**: Achieves the goal of avoiding a separate builder type
4. **GC-friendly**: Immutable strings can be safely shared
5. **Clear Unicode handling**: Explicit `bytes()` vs `chars()` methods
6. **Future-proof**: Can add slices/iterators later without breaking changes

### Negative

1. **Operator inconsistency**: `+=` has different implementation than other types
   - **Mitigation**: Will be generalized via traits
   - **Precedent**: Common in other languages (Python, Java, C#)

2. **Memory overhead**: All Strings carry `capacity` field (8-16 bytes)
   - **Mitigation**: Small relative to typical string sizes
   - **Future optimization**: Could use tagged pointer for small strings

3. **bytes()/chars() copying**: Returns List, requires full copy
   - **Mitigation**: Temporary, will be improved with slices/iterators
   - **Workaround**: Provide `byte_at(i)`, `char_at(i)` for single access

### Trade-offs vs Alternatives

**vs String/MutString split**:

- ✓ Simpler (one type vs two)
- ✓ Better UX (no mental overhead of choosing)
- ✗ Less explicit separation of mutable/immutable

**vs Copy-on-Write**:

- ✓ More predictable performance
- ✓ Feasible with current Wasm GC (no RC introspection needed)
- ✗ Requires explicit `+=` (but this is our goal anyway)

**vs Immutable-only (like JavaScript)**:

- ✓ Efficient mutation via `+=`
- ✓ No need for builder pattern
- ✗ Slightly more complex implementation

### Future Extensions

1. **Trait-based operator overloading**: Generalize `+=` mechanism
2. **Slices**: `s.bytes()` returns `&[u8]` instead of `List<u8>`
3. **Iterators**: `s.chars()` returns lazy iterator
4. **Small String Optimization (SSO)**: Store short strings inline
5. **String interpolation optimization**: Compile-time capacity estimation

### Implementation Notes

The `+=` operator implementation should:

- Use growth factor (typically 2x) for reallocation
- Provide `String::with_capacity(n)` for pre-allocation
- Document performance characteristics clearly
- Consider SSO for strings ≤15 bytes

### Documentation Requirements

User documentation must clarify:

1. Why `s[i]` is prohibited (byte vs char ambiguity)
2. Performance characteristics of `+=` vs `+`
3. Recommended patterns for efficient string building
4. When to use `with_capacity()`

## Related Decisions

- Operator precedence and overloading (to be designed)
- Slice types (to be designed)
- Iterator protocol (to be designed)
- Memory model and move semantics (in progress)

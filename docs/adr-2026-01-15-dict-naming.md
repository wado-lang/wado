# ADR 2026-01-15: Dict Naming

## Status

Accepted

## Context

Wado needs a name for its key-value associative data structure. This type maps directly to the Component Model's `list<tuple<K, V>>` at component boundaries, but internally represents a key-value mapping.

### Type Naming Philosophy in Wado

Wado's type naming strategy follows these principles:

1. **Semantic naming**: Reflect the conceptual type rather than implementation details
2. **UpperCamelCase convention**: User-defined types use `UpperCamelCase` (e.g., `Array`, `String`, `Option`, `Result`)
3. **Brevity with clarity**: Prefer shorter names that remain clear
4. **Zero abstraction to Wasm**: Names should align with Component Model concepts without leaking implementation

### Component Model Mapping

```
| Wado Type   | Component Model Type  | Notes                           |
|-------------|-----------------------|---------------------------------|
| Dict<K, V>  | list<tuple<K, V>>     | As list of tuples at CM boundary|
```

The type is defined by its **semantics** (key-value association), not by how it's represented at the component boundary.

### Survey of Other Programming Languages

A comprehensive survey of dictionary/associative array naming across programming languages (excluding `Map` and `HashMap` due to collision with the `map()` function):

| Name           | Languages                                        | Notes                              |
| -------------- | ------------------------------------------------ | ---------------------------------- |
| **dict/Dict**  | Python, Julia                                    | Most influential; abbreviated form |
| **Dictionary** | C#, Swift, Objective-C (NSDictionary), Smalltalk | Full word; explicit and formal     |
| **Hash**       | Ruby, Perl, Crystal, R (package)                 | Implementation-based naming        |
| **hash-table** | Common Lisp, Racket                              | Hyphenated; formal terminology     |
| **Table**      | Nim, Lua                                         | Table metaphor                     |
| **array**      | PHP (all arrays), D (associative array)          | Generic container term             |

**Key observations:**

- **Dict/Dictionary**: 7 languages, including Python (most widely used scripting language)
- **Implementation-revealing names** (Hash, HashMap): Common but leak implementation details
- **Map**: Widely used but collides with `map()` function, causing ambiguity

### Why Not Map/HashMap?

`Map` and `HashMap` were explicitly excluded from consideration because:

1. **Function collision**: The `map()` function is ubiquitous in functional programming
2. **Ambiguity in code**: `items.map(...)` vs `Map<K, V>` creates confusion
3. **Documentation clarity**: Discussing "maps" becomes ambiguous (function vs type)

This is a well-recognized issue in language design. While languages like Go, Java, and Rust use `Map`/`HashMap`, they accept this ambiguity or rely on context (capitalization) to distinguish them.

## Decision

Adopt **`Dict<K, V>`** as the name for Wado's associative array type.

### Rationale

1. **Balanced brevity and clarity**:
   - Shorter than `Dictionary` (4 chars vs 10 chars)
   - Clearer semantic meaning than `Hash` (which reveals implementation)
   - Unambiguous unlike `Map` (no collision with `map()` function)

2. **Industry precedent**:
   - **Python**: The world's most popular language for beginners and data science uses `dict`
   - **Julia**: A modern scientific computing language uses `Dict`
   - Both languages demonstrate that abbreviated "Dict" is well-understood

3. **Implementation independence**:
   - Unlike `Hash` or `HashMap`, doesn't reveal internal data structure
   - Aligns with "zero abstraction to Wasm" without leaking implementation
   - At Component Model boundary: `list<tuple<K, V>>` (not a hash table)

4. **Type naming consistency**:
   - Follows same pattern as `Array`, `String`, `Option`, `Result`
   - UpperCamelCase, semantic, non-implementation-specific
   - Not overly abbreviated (like `Arr` or `Str` would be)

5. **JavaScript/TypeScript alignment**:
   - Wado uses "ESM-like imports aligning with JavaScript/TypeScript conventions"
   - While JS/TS use `Map`, that option is excluded due to `map()` collision
   - `Dict` is the natural alternative in this context

6. **Python's influence on language design**:
   - Python's design philosophy aligns with Wado's: explicit, readable, practical
   - Python's `dict` is universally recognized and well-loved
   - Adopting `Dict` inherits this positive association

## Alternatives Considered

### Dictionary (Full Word)

**Pros:**

- Completely unambiguous
- Used by C#, Swift, Objective-C, Smalltalk
- No abbreviation confusion

**Cons:**

- Verbose (10 characters)
- Inconsistent with Wado's brevity preference (`String` not `StringType`, `Array` not `ArrayList`)
- Feels heavyweight for such a common type

**Verdict:** Rejected due to verbosity. `Dict` provides the same clarity with better ergonomics.

### Hash

**Pros:**

- Short (4 characters)
- Used by Ruby, Perl, Crystal
- Familiar to many programmers

**Cons:**

- **Leaks implementation details**: Implies hash table implementation
- **Component Model mismatch**: At CM boundary, it's `list<tuple<K, V>>`, not a hash table
- **Contradicts design philosophy**: Wado emphasizes semantic naming over implementation
- Internal implementation may vary (could use tree-based structure for ordered keys, etc.)

**Verdict:** Rejected. Implementation-revealing names contradict Wado's philosophy.

### Table

**Pros:**

- Database/relational metaphor
- Used by Nim, Lua

**Cons:**

- Low adoption (only 2 languages)
- Potential confusion with database tables or HTML tables
- Less intuitive than Dict/Dictionary for key-value mapping

**Verdict:** Rejected due to limited precedent and potential confusion.

### hash-table

**Pros:**

- Formally correct terminology
- Used by Common Lisp, Racket

**Cons:**

- Hyphenated names don't fit Wado's UpperCamelCase convention
- Would need to be `HashTable`, which has same issues as `Hash`
- Implementation-specific

**Verdict:** Rejected for same reasons as `Hash`.

### Map/HashMap

**Pros:**

- Extremely common (Go, Java, C++, Kotlin, Scala, Rust, etc.)
- Well-understood by professional programmers
- Industry standard in many contexts

**Cons:**

- **Fatal flaw**: Collision with `map()` function
  - `items.map(...)` vs `Map<K, V>` creates ambiguity
  - Difficult to discuss in documentation: "map the map" is awkward
  - Search ambiguity: "Wado map" returns both function and type results
- Capitalization-only distinction is fragile (especially in case-insensitive contexts)

**Verdict:** Rejected due to collision with `map()` function. This is a well-known pain point in languages that use this naming.

## Consequences

### Positive

1. **Clear and concise**: `Dict<K, V>` is short, memorable, and semantic
2. **No collision**: Zero ambiguity with `map()` function
3. **Implementation freedom**: Internal implementation can evolve (hash table, tree, rope, etc.) without breaking the semantic contract
4. **Python developers**: Instant familiarity for Python programmers (large community)
5. **Consistent naming**: Aligns with `Array`, `String`, `Option`, `Result` pattern
6. **Documentation clarity**: "dictionary" and "dict" are unambiguous terms

### Negative

1. **Not the most common name**: `Map` is more common in industry (Go, Java, Rust, etc.)
   - **Mitigation**: Python's `dict` is extremely well-known; Julia also uses `Dict`
2. **Abbreviation**: Some might expect full word `Dictionary`
   - **Mitigation**: Python and Julia demonstrate `Dict` is well-understood
3. **Case-sensitive distinction**: `Dict` (type) vs `dict` (Python) requires case awareness
   - **Mitigation**: Wado's UpperCamelCase convention makes this clear

### Documentation Impact

- ✅ Can use "dictionary" in prose without ambiguity
- ✅ `Dict` clearly distinguishes from verb "map" in documentation
- ✅ Type name matches conceptual understanding (key-value dictionary)

### Migration Path

None needed - this is the initial naming decision. No existing code uses a different name.

## References

- [Comparison of programming languages (associative array) - Wikipedia](<https://en.wikipedia.org/wiki/Comparison_of_programming_languages_(associative_array)>)
- [Python dict documentation](https://docs.python.org/3/library/stdtypes.html#dict)
- [Julia Dict documentation](https://docs.julialang.org/en/v1/base/collections/#Dictionaries)
- Component Model specification: list<tuple<K, V>> mapping
- Wado spec.md: Type System - Component Model Mapping table

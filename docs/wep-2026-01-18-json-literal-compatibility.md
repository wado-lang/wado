# WEP: JSON Literal Compatibility

**Date**: 2026-01-18
**Status**: Implemented

## Context

Rust is an excellent systems programming language, but it has a significant limitation: describing complex data structures using object literals is cumbersome. While Rust macros can provide solutions (e.g., `serde_json::json!` macro), Wado has made a deliberate decision not to introduce macros to keep the language simple and maintainable.

Without macros, expressing nested data structures, configuration objects, and test fixtures becomes verbose and error-prone. This would make Wado less productive for common programming tasks, especially in web and application development contexts.

The solution is to make Wado's literal syntax compatible with JSON (JavaScript Object Notation), which is the de facto standard for data interchange on the web. Since JSON syntax is also a subset of JavaScript/TypeScript object literals, this decision provides dual benefits: JSON interoperability and familiarity for web developers.

## Decision

Wado's literal syntax is a **superset of JSON**. Any valid JSON document can be parsed as Wado code, producing dictionaries, arrays, and primitive values.

### What's Shared with JSON

- **Whitespace**: Space (`\u0020`), LF (`\u000A`), CR (`\u000D`), Tab (`\u0009`)
- **String escapes**: `\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`, `\uHHHH`
- **Surrogate pairs**: `"\uD83D\uDE00"` for non-BMP characters
- **Number format**: Decimal integers and floating-point with scientific notation
- **Literals**: `true`, `false`, `null`
- **Object syntax**: `{ "key": value }` with quoted keys
- **List syntax**: `[value, value, ...]`

### Wado Extensions Beyond JSON

While maintaining JSON compatibility, Wado adds developer-friendly extensions:

- **Unquoted keys**: `{ name: "Alice" }` instead of `{ "name": "Alice" }`
- **Shorthand properties**: `{ name, age }` when variable names match keys
- **Trailing commas**: `[1, 2, 3,]` is valid
- **Comments**: `//` and `/* */`
- **Extended Unicode escapes**: `\u{1F600}` for full Unicode range
- **Single-quoted characters**: `'A'` for `char` type (distinct from strings)
- **Numeric separators**: `1_000_000` for readability
- **Numeric prefixes**: `0x`, `0o`, `0b` for hex, octal, binary

These extensions are only available in `.wado` source files, not in imported `.json` files (see WEP: JSON Module Import for strict parsing behavior).

### Tuple Syntax Consistency

An important consequence of JSON compatibility is the choice of tuple syntax. Wado uses `[a, b]` for tuples rather than Rust's `(a, b)`, ensuring consistency with JSON:

```wado
// JSON array maps naturally to Wado tuple
// JSON: [10, 20]
let point: [i32, i32] = [10, 20];

// JSON's heterogeneous arrays map to tuples
// JSON: ["Alice", 25, true]
let user: [String, i32, bool] = ["Alice", 25, true];
```

This design choice prioritizes JSON interoperability over Rust syntax familiarity. The `[...]` syntax creates a visual and conceptual consistency: JSON arrays look the same in Wado code, reducing cognitive load for developers working with JSON data.

For homogeneous collections, explicit type annotation or `as` casting is used:

```wado
let numbers: List<i32> = [1, 2, 3];
let names = ["Alice", "Bob"] as List<String>;
```

## Consequences

### Benefits

**1. Zero Learning Curve for Data Literals**

- Web developers already know JSON syntax
- Copy-paste JSON from APIs, documentation, or web consoles directly into Wado code
- No need to learn a new data description language

**2. Eliminates Need for Macros**

- Complex data structures remain concise without macro magic
- Language stays simpler and more predictable
- Tooling (IDE, formatter, linter) remains straightforward

**3. Ecosystem Integration**

- JSON files can be directly imported as modules (see WEP: JSON Module Import)
- Standard JSON tools (validators, formatters like prettier) work with Wado literals
- Interoperability with JavaScript/TypeScript ecosystems

**4. Type Inference Works Naturally**

- JSON-like literals combine with Wado's type inference
- Concise data definition with type safety

### Trade-offs

**1. Tuple Syntax Diverges from Rust**

- `[a, b]` instead of Rust's `(a, b)`
- May cause minor confusion for Rust developers
- Rationale: JSON compatibility is more valuable for Wado's target use cases

**2. Syntax Constraints from JSON**

- Some syntax choices are constrained by JSON compatibility
- Cannot use `()` for tuples (conflicts with unit type and function calls)
- Must accept JSON's quirks (e.g., no trailing commas in standard JSON, though JSONC allows them)

**3. Distinction Between `.wado` and `.json` Files**

- `.wado` files support all extensions (unquoted keys, trailing commas, comments)
- `.json` files use strict JSON parsing (no extensions allowed)
- This distinction is intentional for ecosystem compatibility (see WEP: JSON Module Import)

## Related

- WEP: JSON Module Import - Importing `.json` files as compile-time modules
- WEP: Tuple and List Literals - Detailed syntax specification for `[...]` literals
- Current implementation: `wado-compiler/src/lexer.rs` (string/number parsing), `wado-compiler/src/parser.rs` (object/array literals)
- Documentation: `docs/json-compatibility.md` (to be deprecated and replaced by this WEP)

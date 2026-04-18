# WEP: JSON Module Import

Date: 2026-01-18
Status: Superseded by [Kiln — Keyed IDL Lowering Notation](./wep-2026-04-12-kiln.md)

JSON compile-time import is no longer a compiler-level feature. The same user-visible syntax (`use config from "./config.json" with { ... }`) is now provided by a Kiln generator (e.g. `core:kiln-json`), which lowers JSON to a Wado module via the `wado:kiln/generator` world. Only the file-extension dispatch and cache logic live in the compiler; parsing, typing, and code emission all move to the generator. The sections below are preserved for historical context.

## Context

Modern applications rely heavily on configuration files, test fixtures, and static data. Traditionally, these are either:

1. Hardcoded in source files (inflexible, requires recompilation)
2. Loaded at runtime from files (requires filesystem access, slower startup, runtime parsing overhead)

For Wasm/WASI applications, runtime file loading has additional challenges:

- Wasm operates in a sandboxed environment with restricted filesystem access
- WASI P3 doesn't guarantee filesystem availability (especially in browser contexts)
- Including a JSON parser increases binary size
- Runtime parsing adds startup latency

Wado's JSON literal compatibility (see WEP: JSON Literal Compatibility) provides a foundation for a better approach: **compile-time JSON module imports**. By loading JSON files during compilation, Wado can provide zero-cost data loading with full type safety.

## Decision

Wado supports importing JSON and JSONC files as compile-time modules using the `use` statement with explicit type annotation.

### Syntax

```wado
// JSON import
use config from "./config.json" with { type: "json" };

// JSONC import (JSON with Comments)
use settings from "./settings.jsonc" with { type: "jsonc" };

// Access namespace members
let app_name = config::name;
let version = config::version;
```

### Strict Parsing Behavior

The Wado compiler enforces **strict parsing** based on the specified file type:

- **`type: "json"`**: RFC 8259 compliant JSON parser
  - Comments (`//`, `/* */`) are **not allowed**
  - Trailing commas are **not allowed**
  - Only quoted keys (`"key": value`)
  - No Wado extensions (numeric separators, hex literals, etc.)

- **`type: "jsonc"`**: JSONC parser (JSON with Comments)
  - Single-line comments (`//`) and multi-line comments (`/* */`) are allowed
  - Trailing commas are allowed
  - Only quoted keys (`"key": value`)
  - No Wado extensions beyond comments and trailing commas

This strict behavior ensures that imported files remain compatible with standard JSON tooling (VSCode, jq, prettier, JSON Schema validators).

### Type Annotation Requirement

For security reasons, JSON imports **require** explicit type annotation:

```wado
// ✅ Correct: explicit type annotation
use config from "./config.json" with { type: "json" };

// ❌ Error: missing type annotation
use config from "./config.json";
```

This requirement prevents accidental execution of malicious code (e.g., a `.json` file that contains executable Wasm or JavaScript disguised as JSON).

### Compile-Time Embedding

JSON data is parsed and embedded into the Wasm binary at compile time:

1. Compiler reads the JSON file
2. Parses it according to the specified type (`json` or `jsonc`)
3. Converts JSON types to Wado types:
   - JSON object → `TreeMap<String, Value>` (or inferred struct type)
   - JSON array → `Array<T>` or tuple `[T, U, V]` (depending on context)
   - JSON string → `String`
   - JSON number → `i32`, `f64`, etc. (inferred from usage)
   - JSON boolean → `bool`
   - JSON null → `Option::None`
4. Embeds the data as Wasm constants
5. Exposes it as a namespace module

There is **no runtime parsing**, no filesystem dependency, and no dynamic type checking.

### Future Extension: Schema Validation

In the future, Wado may support JSON Schema validation at compile time:

```wado
use config from "./config.json" with {
    type: "json",
    schema: "./config.schema.json"
};
```

This would enable:

- Compile-time validation of JSON structure
- Automatic type inference from JSON Schema
- Better error messages for malformed configuration

### Future Extension: Custom Module Loaders

As Wado evolves, users may be able to define custom module loaders for other formats (TOML, YAML, Protocol Buffers, etc.). The `with { type: "..." }` syntax provides extensibility for this future capability.

## Consequences

### Benefits

**1. Zero-Cost Abstraction for Data**

- No runtime JSON parser invocation → avoids including parser code in binary if unused
- No parsing overhead → faster startup time
- Data is Wasm constants → optimal memory layout
- Compile-time type checking → zero runtime type errors

**2. Wasm/WASI Context Advantages**

- No filesystem dependency → works in all Wasm environments (browser, serverless, embedded)
- Deterministic builds → same input always produces same output
- No WASI I/O capability required → can target pure Wasm

**3. Configuration as Code (CaC)**

- Configuration files are statically typed and validated at compile time
- Missing fields, type mismatches, and malformed JSON are **compile errors**, not runtime errors
- Refactoring safety: renaming config fields produces compiler errors
- IDE support: autocomplete, go-to-definition for config fields

**4. Strict Parsing Ensures Ecosystem Compatibility**

- `.json` files remain valid for standard JSON tools (jq, prettier, VSCode validators)
- `.jsonc` files work with JSONC-aware editors (VSCode, Sublime Text)
- Clear contract: `.json` = standard JSON, `.jsonc` = JSON + comments, `.wado` = full Wado syntax
- No vendor lock-in: JSON files can be used by other tools without modification

**5. Security**

- Explicit `with { type: "json" }` prevents accidental execution of non-JSON files
- Consistent with WEP: WebAssembly Module Import Security (all external imports require type annotation)
- Preserves the assumption that `.json` files are **data**, not **code**

**6. Performance Equivalent to Serde at Zero Runtime Cost**

- Similar to Rust's `serde` deserialization, but happens at **compile time**
- No `serde_json::from_str()` overhead at runtime
- Comparable to C/C++ `#include "data.json"` with compile-time parsing

### Trade-offs

**1. Larger Binary Size**

- Embedded data increases Wasm binary size
- Trade-off: binary size vs. no filesystem dependency
- For large datasets, runtime loading may be preferable (future feature)

**2. Recompilation Required for Data Changes**

- Changing a JSON file requires recompiling
- Trade-off: determinism and type safety vs. runtime flexibility
- Suitable for configuration files, not for user-generated data

**3. No Dynamic Schema**

- JSON structure must be known at compile time
- Cannot handle arbitrary JSON APIs without code generation
- Future schema validation may mitigate this

## Alternatives Considered

**1. Runtime JSON Parsing (Rejected)**

- **Cons**: Runtime JSON parser is bundled in stdlib, but using it includes that code in the binary
- **Cons**: Runtime parsing overhead (slower startup)
- **Cons**: Requires `wasi:filesystem` or `wasi:http` (not always available)
- **Cons**: Weak type safety (errors discovered at runtime)

**2. TOML as Primary Config Format (Rejected)**

- **Cons**: Deeply nested structures are verbose and unnatural
- **Cons**: Less familiar to web developers
- **Cons**: Limited tooling compared to JSON

**3. YAML as Primary Config Format (Rejected)**

- **Cons**: Complex specification (indentation-sensitive, anchors/aliases)
- **Cons**: Security concerns (history of arbitrary code execution vulnerabilities)
- **Cons**: No strong standard (multiple incompatible parsers)

**4. JSON5 Support (Rejected)**

- **Cons**: Not a standard (no RFC)
- **Cons**: Limited tooling support
- **Cons**: JSONC is more widely supported (VSCode, TypeScript ecosystem)

**5. Allow Wado Extensions in `.json` Files (Rejected)**

- **Cons**: Breaks compatibility with standard JSON tools
- **Cons**: `.json` files would only be parseable by Wado compiler
- **Cons**: Violates principle of least surprise (`.json` should mean JSON)

## Related

- WEP: JSON Literal Compatibility - Foundation for JSON support in Wado
- WEP: WebAssembly Module Import - Establishes type annotation requirement for external modules
- WEP: Data Section (`__DATA__`) - Alternative for embedding test data in source files
- Current implementation: Not yet implemented (proposed)
- Future work: JSON Schema validation, custom module loaders

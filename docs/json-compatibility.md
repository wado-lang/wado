# JSON Compatibility

Wado's literal syntax is a **superset of JSON**. Any valid JSON document can be parsed as Wado code, producing `Dict`, `Array`, and primitive values.

## What's Shared with JSON

- Whitespace: Space (`\u0020`), LF (`\u000A`), CR (`\u000D`), Tab (`\u0009`)
- String escapes: `\"`, `\\`, `\/`, `\b`, `\f`, `\n`, `\r`, `\t`, `\uHHHH`
- Surrogate pairs: `"\uD83D\uDE00"` for non-BMP characters
- Number format: Decimal integers and floating-point with scientific notation
- Literals: `true`, `false`, `null`
- Object syntax: `{ "key": value }` with quoted keys
- Array syntax: `[value, value, ...]`

## Wado Extensions (Beyond JSON)

- Unquoted keys: `{ name: "Alice" }` instead of `{ "name": "Alice" }`
- Shorthand properties: `{ name, age }` when variable names match keys
- Trailing commas: `[1, 2, 3,]` is valid
- Comments: `//` and `/* */`
- Extended Unicode escapes: `\u{1F600}` for full Unicode range
- Single-quoted characters: `'A'` for `char` type
- Numeric separators: `1_000_000`
- Numeric prefixes: `0x`, `0o`, `0b` for hex, octal, binary

## Importing JSON Files

Use namespace import to load JSON files directly:

```wado
use config from "./config.json" with { type: "json" };

// config is a namespace, so we use :: to access the fields
let name = config::name;
```

JSONC is also supported:

```wado
use config from "./config.jsonc" with { type: "jsonc" };

let name = config::name;
```

JSON and JSONC are loaded at compile time and bundled into the Wasm binary.

### Strict Parsing Behavior

When importing external files, the Wado compiler uses **strict parsing** based on the specified type:

- `type: "json"` - The Wado compiler behaves strictly as a JSON parser (RFC 8259). Comments, trailing commas, and other Wado extensions are **not allowed**.
- `type: "jsonc"` - The Wado compiler behaves strictly as a JSONC parser. JavaScript-style comments (`//` and `/* */`) and trailing commas are allowed in addition to standard JSON syntax.

This strict behavior ensures that imported files remain valid in their respective formats and can be processed by other standard-compliant tools.

## What is JSONC?

JSONC (JSON with Comments) is an extension of JSON that allows comments. While standard JSON (RFC 8259) explicitly prohibits comments, JSONC addresses the need for human-readable configuration files by allowing developers to include explanatory notes.

JSONC was informally introduced by Microsoft for Visual Studio Code configuration files (e.g., `settings.json`, `launch.json`) and has since become a de facto standard for configuration files.

### JSONC Syntax

JSONC extends JSON with:

- **Single-line comments**: Start with `//` and extend to the end of the line
- **Multi-line comments**: Start with `/*` and end with `*/`
- **Trailing commas**: Allowed after the last element in arrays and objects

```jsonc
{
  // This is a single-line comment
  "name": "example",
  /*
    This is a multi-line comment
    explaining the version field.
  */
  "version": "1.0.0",
}
```

**Note:** JSONC does **not** allow unquoted keys or single-quoted strings. These are Wado-specific extensions that are only available when parsing `.wado` files.

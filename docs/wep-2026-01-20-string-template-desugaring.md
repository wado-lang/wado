# String Template Desugaring

## Context

String templates (`` `Hello, {name}!` ``) are currently lowered in the resolver to chained `String::concat` calls. This approach has limitations:

1. No support for tagged templates (`sql`...``, `String::raw`...``)
2. Format specifiers are not fully implemented
3. No access to raw (unescaped) string literals
4. Inefficient for templates with many interpolations

We need a unified desugaring strategy that:

- Supports tagged templates like JavaScript
- Enables `String::raw` and `String::base64` as regular functions
- Requires compile-time tuple enumeration for heterogeneous values (see [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md))

## Decision

### String Parts Types

Tag function signatures determine which form of string parts is provided. Two newtype aliases are defined:

```wado
type CookedStrings = Array<String>;  // Escape sequences processed (\n -> newline)
type RawStrings = Array<String>;     // Escape sequences preserved (\n -> "\\n")
```

The compiler inspects the tag function's first parameter type and generates only the needed form:

| First parameter type | What the compiler emits | Use case |
|---|---|---|
| `CookedStrings` | Cooked strings only | Most tagged templates |
| `RawStrings` | Raw strings only | `String::raw` |

Untagged templates are special-cased by the compiler and do not construct any array (see below).

Invariant: `strings.len() == values.len() + 1`

### Untagged Template Desugaring

Untagged templates are special-cased by the compiler for efficiency. No `CookedStrings` array or values tuple is constructed.

```wado
`Hello, {name}! You are {age}.`
```

The compiler directly emits an efficient append sequence using a mutable string and labeled block expression. Conceptually equivalent to:

```wado
__tmpl: {
    let mut __r = "Hello, ";
    __r.append(name.to_string());
    __r.append("! You are ");
    __r.append(age.to_string());
    __r.append(".");
    __r
}
```

### Tagged Template Desugaring

```wado
sql`SELECT * FROM users WHERE id = {id} AND name = {name}`
```

Desugars to:

```wado
__tmpl: {
    let __strings = CookedStrings::from(["SELECT * FROM users WHERE id = ", " AND name = ", ""]);
    let __values = [id, name];
    sql(__strings, __values)
}
```

The tag function is a generic function that receives the values as a tuple, preserving each value's original type:

```wado
fn sql<Values>(strings: CookedStrings, values: Values) -> SqlQuery {
    let mut query = strings[0];
    let mut params: Array<SqlParam> = [];
    for let [i, v] of values.entries() {
        params.append(v.to_sql_param());
        query.append((i + 1).to_string());
        query.append(strings[i + 1]);
    }
    return SqlQuery { query, params };
}
```

The `for let [i, v] of values.entries()` is compile-time tuple enumeration. See [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md) for the full specification.

### `String::raw` Implementation

`String::raw` is a regular static method. Its first parameter is `RawStrings`, so the compiler emits raw (unescaped) strings:

```wado
impl String {
    fn raw<Values>(strings: RawStrings, values: Values) -> String {
        let mut result = strings[0];
        for let [i, v] of values.entries() {
            result.append(v.to_string());
            result.append(strings[i + 1]);
        }
        return result;
    }
}
```

Usage:

```wado
String::raw`Hello\nWorld`     // -> "Hello\\nWorld" (12 chars, not 11)
String::raw`Path: {path}\n`   // -> "Path: " + to_string(path) + "\\n"
```

### `String::base64` Implementation

Compile-time base64 decoding. Uses `CookedStrings` since there are no escape sequences to preserve:

```wado
impl String {
    fn base64(strings: CookedStrings, values: []) -> Array<u8> {
        // values must be empty (no interpolation allowed)
        // Decoded at compile time
        return __builtin_base64_decode__(strings[0]);
    }
}
```

Usage:

```wado
let bytes = String::base64`SGVsbG8=`;  // -> [72, 101, 108, 108, 111]
```

Compile error if interpolation is present.

### Format Specifiers

When a format specifier is present, the value is pre-formatted before being passed to the tag:

```wado
fmt`Value: {pi:.2}`
```

Desugars to:

```wado
__tmpl: {
    let __strings = CookedStrings::from(["Value: ", ""]);
    let __values = [__format__(pi, ".2")];
    fmt(__strings, __values)
}
```

For untagged templates, format specifiers are applied during the direct stringification.

### Brace Escaping

`{{` and `}}` produce literal `{` and `}`:

```wado
`JSON: {{"key": {value}}}`
// cooked[0] = "JSON: {\"key\": "
// cooked[1] = "}"
// values = [value]
```

Lexer change required: `{{` -> `{`, `}}` -> `}` in template strings.

### Edge Cases

| Case                    | Input                   | Output                         |
| ----------------------- | ----------------------- | ------------------------------ |
| Empty template          | `` ` ` ``               | `""`                           |
| No interpolation        | `` `hello` ``           | `"hello"`                      |
| Only interpolation      | `` `{x}` ``             | `to_string(x)`                 |
| Adjacent interpolations | `` `{a}{b}` ``          | `strings = ["", "", ""]`       |
| Escaped braces          | `` `{{x}}` ``           | `"{x}"` (literal)              |
| Nested template         | `` `outer {`inner`}` `` | Inner template evaluated first |
| Multiline               | Preserved               | Newlines in cooked/raw         |

## Implementation Plan

### Phase 1: Foundation

- [ ] Add `{{` and `}}` escape support in lexer
- [ ] Store both cooked and raw strings in `TemplatePart::String`
- [ ] Define `CookedStrings` and `RawStrings` newtypes in `core:internal`

### Phase 2: Untagged Template Optimization

- [ ] Improve untagged template lowering for efficiency (buffer-based or `StringBuilder`)
- [ ] Apply format specifiers during direct stringification

### Phase 3: Tagged Templates

- [ ] Implement tag function lookup and signature-based string selection
- [ ] Generate `CookedStrings` or `RawStrings` based on tag function's first parameter type
- [ ] Pass values as tuple to tag function
- [ ] Implement `String::raw` using `RawStrings` and tuple enumeration
- [ ] Implement `String::base64` with compile-time decoding

### Phase 4: Brace Escaping

- [ ] Implement `{{` / `}}` lexer support

## Consequences

### Positive

- Unified model for all template strings
- Tagged templates enable DSLs (SQL, regex, etc.)
- `String::raw` works naturally without parser special-casing
- Type-safe interpolation values in tagged templates
- Signature-based string selection avoids unnecessary wasm bloat (no unused raw/cooked arrays)

### Negative

- Depends on compile-time tuple enumeration (separate WEP)
- Breaking change if existing code relies on current lowering
- Two newtype aliases (`CookedStrings`, `RawStrings`) for conceptually similar data

### Risks

- Compile-time tuple enumeration may interact unexpectedly with other features
- Tag function signature inspection adds complexity to the compiler

## Related WEPs

- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md): Required for iterating over heterogeneous values in tag functions
- [Tagged Template Literals for Compile-Time Execution](./wep-2026-01-10-tagged-template-literals.md): Covers compile-time evaluation of tag functions

## References

- [MDN: Template literals](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
- [MDN: String.raw()](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/String/raw)
- [TypeScript: Template Literal Types](https://www.typescriptlang.org/docs/handbook/2/template-literal-types.html)

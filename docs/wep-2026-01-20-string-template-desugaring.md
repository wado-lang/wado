# String Template Desugaring

## Context

String templates (`` `Hello, {name}!` ``) are currently lowered in the elaborator to chained `+` (Add::add) calls. This approach has limitations:

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
type CookedStrings = List<String>;  // Escape sequences processed (\n -> newline)
type RawStrings = List<String>;     // Escape sequences preserved (\n -> "\\n")
```

The compiler inspects the tag function's first parameter type and generates only the needed form:

| First parameter type | What the compiler emits | Use case              |
| -------------------- | ----------------------- | --------------------- |
| `CookedStrings`      | Cooked strings only     | Most tagged templates |
| `RawStrings`         | Raw strings only        | `String::raw`         |

Untagged templates are special-cased by the compiler and do not construct any array (see below).

Invariant: `strings.len() == values.len() + 1`

### Untagged Template Desugaring

Untagged templates are special-cased by the compiler for efficiency. No `CookedStrings` array or values tuple is constructed.

```wado
`Hello, {name}! You are {age}.`
```

The compiler directly emits an efficient sequence using a mutable string and labeled block expression. All interpolations go through `Formatter` + `Display::fmt` (see [Format Traits](./wep-2026-02-01-format-traits.md)) for predictable behavior, regardless of whether a format specifier is present. `Formatter` wraps `&mut String` and writes directly into the output buffer with no intermediate allocations.

```wado
__tmpl: {
    let mut __r = "Hello, ";
    name.fmt(&mut Formatter::new(&mut __r, FormatSpec::default()));
    __r.push_str("! You are ");
    age.fmt(&mut Formatter::new(&mut __r, FormatSpec::default()));
    __r.push_str(".");
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
    let mut params: List<SqlParam> = [];
    for let [i, v] of values.enumerate() {
        params.push(v.to_sql_param());
        query.push_str("?");
        query.push_str(strings[i + 1]);
    }
    return SqlQuery { query, params };
}
```

The `for let [i, v] of values.enumerate()` is compile-time tuple enumeration. See [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md) for the full specification.

### `String::raw` Implementation

`String::raw` is a regular static method. Its first parameter is `RawStrings`, so the compiler emits raw (unescaped) strings:

```wado
impl String {
    fn raw<Values>(strings: RawStrings, values: Values) -> String {
        let mut result = strings[0];
        for let [i, v] of values.enumerate() {
            v.fmt(&mut Formatter::new(&mut result, FormatSpec::default()));
            result.push_str(strings[i + 1]);
        }
        return result;
    }
}
```

Usage:

```wado
String::raw`Hello\nWorld`     // -> "Hello\\nWorld" (12 chars, not 11)
String::raw`Path: {path}\n`   // -> "Path: " + display(path) + "\\n"
```

### `String::base64` Implementation

Compile-time base64 decoding. Uses `CookedStrings` since there are no escape sequences to preserve:

```wado
impl String {
    fn base64(strings: CookedStrings, values: []) -> List<u8> {
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

All interpolations use the `Formatter` infrastructure (see [Format Traits](./wep-2026-02-01-format-traits.md)). When a format specifier is present, it becomes a custom `FormatSpec`; otherwise `FormatSpec::default()` is used. This ensures consistent behavior regardless of whether a specifier is present.

```wado
`Pi is {pi:.2}`
```

Desugars to:

```wado
__tmpl: {
    let mut __r = "Pi is ";
    pi.fmt(&mut Formatter::new(&mut __r, FormatSpec { precision: Option::<i32>::Some(2), ..FormatSpec::default() }));
    __r
}
```

For tagged templates, format-specifier interpolations are pre-formatted and passed as strings in the values tuple:

```wado
fmt`Value: {pi:.2}`
```

Desugars to:

```wado
__tmpl: {
    let __strings = CookedStrings::from(["Value: ", ""]);
    let mut __formatted = "";
    pi.fmt(&mut Formatter::new(&mut __formatted, FormatSpec { precision: Option::<i32>::Some(2), ..FormatSpec::default() }));
    let __values = [__formatted];
    fmt(__strings, __values)
}
```

Note: format specifiers are resolved at the call site before the tag function receives them. The tag function sees pre-formatted strings in the values tuple.

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
| Only interpolation      | `` `{x}` ``             | `Display::fmt` of x            |
| Adjacent interpolations | `` `{a}{b}` ``          | `strings = ["", "", ""]`       |
| Escaped braces          | `` `{{x}}` ``           | `"{x}"` (literal)              |
| Nested template         | `` `outer {`inner`}` `` | Inner template evaluated first |
| Multiline               | Preserved               | Newlines in cooked/raw         |

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

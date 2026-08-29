# WEP: String Template Desugaring

## Context

String templates (`` `Hello, ${name}!` ``) are currently lowered in the elaborator to chained `+` (Add::add) calls. This approach has limitations:

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
`Hello, ${name}! You are ${age}.`
```

The compiler directly emits an efficient sequence using a mutable string and labeled block expression. Every interpolation goes through a `Formatter` (see [Format Traits](./wep-2026-02-01-format-traits.md)), specifier or not. `Formatter` wraps `&mut String` and writes into the output buffer with no intermediate allocation.

```wado
__tmpl: {
    let mut __r = "Hello, ";
    name.fmt(&mut Formatter::new(&mut __r));
    __r.push_str("! You are ");
    age.fmt(&mut Formatter::new(&mut __r));
    __r.push_str(".");
    __r
}
```

One `Formatter` local serves the whole block; the snippets spell out a fresh
one per interpolation for readability.

### Tagged Template Desugaring

```wado
sql`SELECT * FROM users WHERE id = ${id} AND name = ${name}`
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
            v.fmt(&mut Formatter::new(&mut result));
            result.push_str(strings[i + 1]);
        }
        return result;
    }
}
```

Usage:

```wado
String::raw`Hello\nWorld`     // -> "Hello\\nWorld" (12 chars, not 11)
String::raw`Path: ${path}\n`   // -> "Path: " + display(path) + "\\n"
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

A specifier selects the trait method to call (`fmt`, `fmt_lower_hex`, …) and
the `Formatter` to call it with. The compiler emits `Formatter::new` when the
specifier sets no field beyond the type — width, precision, fill, alignment,
`+`, `#` and `0` are what make it a field-by-field literal instead:

```wado
`Pi is ${pi:.2}`
```

Desugars to:

```wado
__tmpl: {
    let mut __r = "Pi is ";
    pi.fmt(&mut Formatter {
        fill: ' ', align: Alignment::Right, sign_plus: false, alternate: false,
        zero_pad: false, width: -1, precision: 2, indent: 0, buf: &mut __r,
    });
    __r
}
```

The literal writes every field, sentinels included, rather than deriving from
`Formatter::new`. See [Format Traits](./wep-2026-02-01-format-traits.md) for
the field list.

For tagged templates, format-specifier interpolations are pre-formatted and passed as strings in the values tuple:

```wado
fmt`Value: ${pi:.2}`
```

Desugars to:

```wado
__tmpl: {
    let __strings = CookedStrings::from(["Value: ", ""]);
    let mut __formatted = "";
    pi.fmt(&mut Formatter {
        fill: ' ', align: Alignment::Right, sign_plus: false, alternate: false,
        zero_pad: false, width: -1, precision: 2, indent: 0,
        buf: &mut __formatted,
    });
    let __values = [__formatted];
    fmt(__strings, __values)
}
```

Note: format specifiers are resolved at the call site before the tag function receives them. The tag function sees pre-formatted strings in the values tuple.

### Braces and `$` Escaping

Only `${` opens an interpolation, so `{` and `}` are literal text and JSON-like
content needs no escaping:

```wado
`JSON: {"key": ${value}}`
// cooked[0] = "JSON: {\"key\": "
// cooked[1] = "}"
// values = [value]
```

A literal `$` is also plain text; escape it with `\$` only when it directly
precedes a `{` that should stay literal (`` `\${x}` `` renders `${x}`).

### Edge Cases

| Case                    | Input                    | Output                         |
| ----------------------- | ------------------------ | ------------------------------ |
| Empty template          | `` ` ` ``                | `""`                           |
| No interpolation        | `` `hello` ``            | `"hello"`                      |
| Only interpolation      | `` `${x}` ``             | `Display::fmt` of x            |
| Adjacent interpolations | `` `${a}${b}` ``         | `strings = ["", "", ""]`       |
| Literal braces          | `` `{x}` ``              | `"{x}"` (no interpolation)     |
| Literal `${`            | `` `\${x}` ``            | `"${x}"` (literal)             |
| Nested template         | `` `outer ${`inner`}` `` | Inner template evaluated first |
| Multiline               | Preserved                | Newlines in cooked/raw         |

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

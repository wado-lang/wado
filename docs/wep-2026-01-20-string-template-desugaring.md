# String Template Desugaring

## Context

String templates (`` `Hello, {name}!` ``) are currently lowered in the resolver to chained `string_concat` calls. This approach has limitations:

1. No support for tagged templates (`sql`...``, `String::raw`...``)
2. Format specifiers are not fully implemented
3. No access to raw (unescaped) string literals
4. Inefficient for templates with many interpolations

We need a unified desugaring strategy that:
- Supports tagged templates like JavaScript
- Provides both cooked and raw string literals
- Enables `String::raw` and `String::base64` as regular functions
- Requires compile-time tuple enumeration for heterogeneous values

## Decision

### TemplateStrings Structure

All template literals produce a `TemplateStrings` struct containing both processed (cooked) and unprocessed (raw) string parts:

```wado
struct TemplateStrings {
    cooked: Array<String>,  // Escape sequences processed (\n → newline)
    raw: Array<String>,     // Escape sequences preserved (\n → "\\n")
}
```

Invariant: `cooked.len() == raw.len() == values.len() + 1`

### Standard Template Desugaring

```wado
`Hello, {name}! You are {age}.`
```

Desugars to:

```wado
{
    let __strings = TemplateStrings {
        cooked: ["Hello, ", "! You are ", "."],
        raw: ["Hello, ", "! You are ", "."],
    };
    let __values = [name, age];
    __default_template__(__strings, __values)
}
```

Where `__default_template__` is implemented as:

```wado
fn __default_template__<T>(strings: TemplateStrings, values: T) -> String {
    let mut result = strings.cooked[0];
    for let [i, v] of values {
        result += to_string(v);
        result += strings.cooked[i + 1];
    }
    return result;
}
```

### Tagged Template Desugaring

```wado
sql`SELECT * FROM users WHERE id = {id} AND name = {name}`
```

Desugars to:

```wado
{
    let __strings = TemplateStrings {
        cooked: ["SELECT * FROM users WHERE id = ", " AND name = ", ""],
        raw: ["SELECT * FROM users WHERE id = ", " AND name = ", ""],
    };
    let __values = [id, name];
    sql(__strings, __values)
}
```

The tag function receives the raw values (not stringified), enabling type-safe query building.

### `String::raw` Implementation

`String::raw` is a regular static method, not special parser syntax:

```wado
impl String {
    fn raw<T>(strings: TemplateStrings, values: T) -> String {
        let mut result = strings.raw[0];  // Uses raw, not cooked
        for let [i, v] of values {
            result += to_string(v);
            result += strings.raw[i + 1];
        }
        return result;
    }
}
```

Usage:
```wado
String::raw`Hello\nWorld`     // → "Hello\\nWorld" (12 chars, not 11)
String::raw`Path: {path}\n`   // → "Path: " + to_string(path) + "\\n"
```

### `String::base64` Implementation

Compile-time base64 decoding:

```wado
impl String {
    fn base64(strings: TemplateStrings, values: []) -> Array<u8> {
        // values must be empty (no interpolation allowed)
        // Decoded at compile time
        return __builtin_base64_decode__(strings.raw[0]);
    }
}
```

Usage:
```wado
let bytes = String::base64`SGVsbG8=`;  // → [72, 101, 108, 108, 111]
```

Compile error if interpolation is present.

### Format Specifiers

When a format specifier is present, the value is stringified before being passed to the tag:

```wado
fmt`Value: {pi:.2}`
```

Desugars to:

```wado
{
    let __strings = TemplateStrings { ... };
    let __values = [__format__(pi, ".2")];  // Pre-formatted
    fmt(__strings, __values)
}
```

For standard templates, format specifiers are applied during stringification:

```wado
`Pi is {pi:.2}`
// → __default_template__(strings, [pi])
// with format spec passed to to_string or dedicated formatter
```

### Brace Escaping

`{{` and `}}` produce literal `{` and `}`:

```wado
`JSON: {{"key": {value}}}`
// cooked[0] = "JSON: {\"key\": "
// cooked[1] = "}"
// values = [value]
```

Lexer change required: `{{` → `{`, `}}` → `}` in template strings.

### Compile-Time Tuple Enumeration

Tuple `for-of` always yields `[index, value]` pairs, unlike Array `for-of` which yields values only. This reflects the fundamental difference between tuple and array iteration:

- **Array**: Runtime loop, homogeneous types, index is runtime value
- **Tuple**: Compile-time unrolling, heterogeneous types, index is compile-time constant

```wado
// Tuple: always [index, value]
for let [i, v] of tuple_expr {
    // i: compile-time constant index
    // v: tuple_expr[i], type varies per iteration
}

// Array: value only (like JavaScript)
for let v of array_expr {
    // v: element value
}
```

Expansion example:

```wado
let t: [i32, String, f64] = [1, "hi", 3.14];
for let [i, v] of t {
    println(`{i}: {v}`);
}
```

Becomes:

```wado
{
    let i = 0;
    let v = t.0;  // v: i32
    println(`{i}: {v}`);
}
{
    let i = 1;
    let v = t.1;  // v: String
    println(`{i}: {v}`);
}
{
    let i = 2;
    let v = t.2;  // v: f64
    println(`{i}: {v}`);
}
```

Value-only iteration (discard index):

```wado
for let [_, v] of tuple {
    // use v only
}
```

Constraints:
- `tuple_expr` must have a tuple type
- Loop body must be valid for each element type (checked after expansion)
- `break` and `continue` are not allowed (compile error)
- `i` is a compile-time constant, usable in array/tuple indexing

### Edge Cases

| Case | Input | Output |
|------|-------|--------|
| Empty template | `` ` ` `` | `""` |
| No interpolation | `` `hello` `` | `"hello"` |
| Only interpolation | `` `{x}` `` | `to_string(x)` |
| Adjacent interpolations | `` `{a}{b}` `` | `strings = ["", "", ""]` |
| Escaped braces | `` `{{x}}` `` | `"{x}"` (literal) |
| Nested template | `` `outer {`inner`}` `` | Inner template evaluated first |
| Multiline | Preserved | Newlines in cooked/raw |

## Implementation Plan

### Phase 1: Foundation

- [ ] Add `{{` and `}}` escape support in lexer
- [ ] Store both cooked and raw strings in `TemplatePart::String`
- [ ] Define `TemplateStrings` struct in `core:internal`

### Phase 2: Desugaring

- [ ] Implement template desugaring in `desugar.rs`
- [ ] Generate `TemplateStrings` literal and values tuple
- [ ] Call `__default_template__` for untagged templates

### Phase 3: Compile-Time Tuple Enumeration

- [ ] Parse `for let [i, v] of expr` syntax
- [ ] Implement loop unrolling in desugar phase
- [ ] Type-check each unrolled block independently

### Phase 4: Tagged Templates

- [ ] Implement `String::raw` using tuple enumeration
- [ ] Implement `String::base64` with compile-time decoding
- [ ] Add format specifier handling

### Phase 5: Optimization

- [ ] Constant-fold templates with only literals
- [ ] Inline small templates without function call overhead

## Consequences

### Positive

- Unified model for all template strings
- Tagged templates enable DSLs (SQL, regex, etc.)
- `String::raw` works naturally without parser special-casing
- Type-safe interpolation values in tagged templates
- Compile-time tuple enumeration has broader applications

### Negative

- Compile-time tuple enumeration adds complexity to the compiler
- TemplateStrings struct adds runtime overhead (two arrays)
- Breaking change if existing code relies on current lowering

### Risks

- Compile-time tuple enumeration may interact unexpectedly with other features
- Performance of unrolled loops for large tuples needs monitoring

## Related WEPs

- [Tagged Template Literals for Compile-Time Execution](./wep-2026-01-10-tagged-template-literals.md): Covers compile-time evaluation of tag functions. This WEP provides the desugaring mechanism that feeds into that compile-time execution model.

## References

- [MDN: Template literals](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Template_literals)
- [MDN: String.raw()](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/String/raw)
- [TypeScript: Template Literal Types](https://www.typescriptlang.org/docs/handbook/2/template-literal-types.html)

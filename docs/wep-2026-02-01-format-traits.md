# WEP: Format Traits

## Context

Template strings in Wado support format specifiers: `` `{x:spec}` ``. As defined in [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md), Wado uses Rust-compatible format specifiers:

| Specifier | Description           | Example              |
| --------- | --------------------- | -------------------- |
| (none)    | Default display       | `{x}` → `"42"`       |
| `?`       | Debug/Inspect         | `{x:?}` → `"42"`     |
| `b`       | Binary integers       | `{x:b}` → `"101010"` |
| `o`       | Octal integers        | `{x:o}` → `"52"`     |
| `x`       | Lowercase hex         | `{x:x}` → `"2a"`     |
| `X`       | Uppercase hex         | `{x:X}` → `"2A"`     |
| `e`       | Lowercase exponential | `{x:e}` → `"4.2e1"`  |
| `E`       | Uppercase exponential | `{x:E}` → `"4.2E1"`  |

This WEP defines the trait system and Formatter infrastructure that backs these format specifiers.

## Decision

### Formatter Infrastructure

Format traits write to a `Formatter` object that holds format options and a reference to the output buffer. The `Formatter` does not own its buffer; it writes directly into the caller's `&mut String`. Format specification fields are embedded directly into `Formatter` to avoid an extra GC struct allocation.

```wado
/// Text alignment for padding
variant Alignment {
    Left,
    Center,
    Right,
}

/// Formatter that writes directly into a referenced output buffer.
/// Format specification fields are embedded to save one GC struct allocation.
struct Formatter {
    fill: char,
    align: Alignment,
    sign_plus: bool,
    alternate: bool,
    zero_pad: bool,
    width: i32,        // -1 = not specified
    precision: i32,    // -1 = not specified
    buf: &mut String,
}

impl Formatter {
    fn new(buf: &mut String) -> Formatter;
    fn write_str(&mut self, s: &String);
    fn write_char(&mut self, c: char);
    fn width(&self) -> i32;
    fn precision(&self) -> i32;
    fn alternate(&self) -> bool;
    fn sign_plus(&self) -> bool;
}
```

### Format Traits

All format traits follow the same pattern: write to a `Formatter`.

```wado
trait Display {
    fn fmt(&self, f: &mut Formatter);
}

trait Binary {
    fn fmt(&self, f: &mut Formatter);
}

trait Octal {
    fn fmt(&self, f: &mut Formatter);
}

trait LowerHex {
    fn fmt(&self, f: &mut Formatter);
}

trait UpperHex {
    fn fmt(&self, f: &mut Formatter);
}

trait LowerExp {
    fn fmt(&self, f: &mut Formatter);
}

trait UpperExp {
    fn fmt(&self, f: &mut Formatter);
}
```

### Debug Formatting

The `:?` specifier resolves to the `Inspect` trait. The `:#?` specifier resolves to the `InspectAlt` trait for pretty-printed (indented multi-line) output. The compiler auto-synthesizes `Inspect` and `InspectAlt` for all user types; types can provide custom implementations to override.

```wado
trait Inspect {
    fn inspect(&self, f: &mut Formatter);
}

trait InspectAlt {
    fn inspect_alt(&self, f: &mut Formatter);
}
```

### Display Derivation

`Display` comes from the type's own `impl Display`. The compiler provides one
for the two type kinds whose string form is unambiguous: a plain `enum` displays
its bare case name (`Red`, vs `Inspect`'s `Color::Red`), and a newtype inherits
its base type's `Display` transparently (`Meters = f64` renders `3.14`). Any
other type — a struct, variant, or generic container — needs a hand-written
`impl Display`; `{x}` on a type without one is a compile error (use `{x:?}`).
`DisplayAlt` (`{x:#}`) follows `Display`. See
[Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### Alternate (Alt) Trait Variants

Each format trait has an alternate variant activated by the `#` flag. For Inspect, `InspectAlt` produces pretty-printed output. For numeric traits, Alt variants add prefixes (`0x`, `0b`, `0o`).

| Base Trait | Alt Trait     | Syntax  | Effect                        |
| ---------- | ------------- | ------- | ----------------------------- |
| `Display`  | `DisplayAlt`  | `{:#}`  | Delegates to `Display`        |
| `Inspect`  | `InspectAlt`  | `{:#?}` | Pretty-print with indentation |
| `Binary`   | `BinaryAlt`   | `{:#b}` | Add `0b` prefix               |
| `Octal`    | `OctalAlt`    | `{:#o}` | Add `0o` prefix               |
| `LowerHex` | `LowerHexAlt` | `{:#x}` | Add `0x` prefix               |
| `UpperHex` | `UpperHexAlt` | `{:#X}` | Add `0X` prefix               |

### Format Resolution

| Specifier | Resolution                                             |
| --------- | ------------------------------------------------------ |
| (none)    | `Display::fmt` (compile error if `T` has no `Display`) |
| `?`       | `Inspect::inspect`                                     |
| `#`       | `DisplayAlt::fmt_alt`                                  |
| `#?`      | `InspectAlt::inspect_alt`                              |
| `b`       | `Binary::fmt`                                          |
| `o`       | `Octal::fmt`                                           |
| `x`       | `LowerHex::fmt`                                        |
| `X`       | `UpperHex::fmt`                                        |
| `e`       | `LowerExp::fmt`                                        |
| `E`       | `UpperExp::fmt`                                        |

### Primitive Implementations

| Type           | Traits                                               |
| -------------- | ---------------------------------------------------- |
| Integer types  | `Display`, `Binary`, `Octal`, `LowerHex`, `UpperHex` |
| Float types    | `Display`, `LowerExp`, `UpperExp`                    |
| `bool`, `char` | `Display`                                            |
| `String`       | `Display`                                            |

### Zero Padding

Zero padding (`{x:08}`) inserts zeros after sign/prefix but before digits:

```
{-42:08}   → "-0000042"
{42:#08x}  → "0x00002a"
```

## Consequences

### Positive

1. **Accurate formatting**: Precision is available during formatting
2. **Efficient**: Write directly to buffer
3. **Rust-compatible**: Familiar design for Rust developers
4. **Extensible**: Easy to add new format options or traits

### Negative

1. **Infrastructure complexity**: Requires `Formatter` and `Alignment` types
2. **Implementation effort**: All primitive formatting needs trait implementations

### Implemented Extensions

1. **`{:#?}`**: Pretty-print debug with indentation via `InspectAlt` trait

### Future Extensions

1. **Dynamic width/precision**: `{value:{width}.{precision}}`

## References

- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust std::fmt module](https://doc.rust-lang.org/std/fmt/index.html)

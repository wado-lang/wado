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

Format traits write to a `Formatter` object that holds format options and an output buffer. This design follows Rust's `std::fmt` approach, adapted for Wado's semantics.

```wado
/// Text alignment for padding
enum Alignment {
    Left,
    Center,
    Right,
}

/// Format specification parsed from `{expr:spec}`
struct FormatSpec {
    /// Minimum width (e.g., `{x:8}` → width = 8)
    width: Option<i32>,
    /// Precision for floats or max width for strings (e.g., `{x:.2}`)
    precision: Option<i32>,
    /// Fill character for padding (default: space)
    fill: char,
    /// Text alignment (default: Right for numbers, Left for strings)
    align: Alignment,
    /// Always show sign for positive numbers (`+` flag)
    sign_plus: bool,
    /// Use alternate form (`#` flag: 0x, 0b, 0o prefixes)
    alternate: bool,
    /// Zero-pad numbers (`0` flag, implies right-align)
    zero_pad: bool,
}

/// Formatter that accumulates formatted output
struct Formatter {
    spec: FormatSpec,
    buf: String,
}
```

### Formatter Methods

```wado
impl Formatter {
    /// Write a string to the output buffer
    fn write_str(&mut self, s: String);

    /// Write a single character to the output buffer
    fn write_char(&mut self, c: char);

    /// Access the format specification
    fn spec(&self) -> &FormatSpec;

    /// Check if width is specified
    fn width(&self) -> Option<i32>;

    /// Check if precision is specified
    fn precision(&self) -> Option<i32>;

    /// Check if alternate form is requested
    fn alternate(&self) -> bool;

    /// Check if sign should always be shown
    fn sign_plus(&self) -> bool;

    /// Finish formatting and return the result with padding applied
    fn finish(&mut self) -> String;
}
```

### Format Traits

All format traits follow the same pattern: write to a `Formatter`.

```wado
/// Default display formatting - used by `{expr}`
trait Display {
    fn fmt(&self, f: &mut Formatter);
}

/// Binary formatting - used by `{expr:b}`
trait Binary {
    fn fmt(&self, f: &mut Formatter);
}

/// Octal formatting - used by `{expr:o}`
trait Octal {
    fn fmt(&self, f: &mut Formatter);
}

/// Lowercase hex formatting - used by `{expr:x}`
trait LowerHex {
    fn fmt(&self, f: &mut Formatter);
}

/// Uppercase hex formatting - used by `{expr:X}`
trait UpperHex {
    fn fmt(&self, f: &mut Formatter);
}

/// Lowercase exponential formatting - used by `{expr:e}`
trait LowerExp {
    fn fmt(&self, f: &mut Formatter);
}

/// Uppercase exponential formatting - used by `{expr:E}`
trait UpperExp {
    fn fmt(&self, f: &mut Formatter);
}
```

### Method Name: `fmt` for All Traits

Unlike the previous design with distinct method names (`fmt_binary`, `fmt_lower_hex`, etc.), all traits use the same method name `fmt`. This works because:

1. **Trait dispatch**: The compiler knows which trait to use based on the format specifier
2. **No ambiguity**: A type can implement multiple format traits, and the correct one is selected at compile time
3. **Simpler implementation**: Consistent interface across all format traits
4. **Rust compatibility**: Matches Rust's `std::fmt` design

### Debug Formatting

The `:?` specifier uses the compiler intrinsic `builtin::inspect()` and does not use a trait. This provides universal debug formatting for all types without requiring trait implementation.

### Format Resolution

When the compiler processes `{expr:spec}`:

1. Parse the format specification into `FormatSpec`
2. Create a `Formatter` with the spec
3. Dispatch to the appropriate trait based on specifier:

| Specifier | Resolution                              |
| --------- | --------------------------------------- |
| (none)    | `Display::fmt` or `builtin::inspect`    |
| `?`       | `builtin::inspect` (always)             |
| `b`       | `Binary::fmt`                           |
| `o`       | `Octal::fmt`                            |
| `x`       | `LowerHex::fmt`                         |
| `X`       | `UpperHex::fmt`                         |
| `e`       | `LowerExp::fmt`                         |
| `E`       | `UpperExp::fmt`                         |

4. Call `formatter.finish()` to apply padding and get the result

### Example: Display Resolution

For `{expr}` (no specifier):

1. If `expr` is `String` → use directly (no formatting needed)
2. If `expr: Display` → call `Display::fmt(&expr, &mut formatter)`
3. Otherwise → call `builtin::inspect(expr)`

### Primitive Implementations

Integer types (`i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`) implement:
- `Display`, `Binary`, `Octal`, `LowerHex`, `UpperHex`

Float types (`f32`, `f64`) implement:
- `Display`, `LowerExp`, `UpperExp`

Other primitives:
- `bool`: `Display` only
- `char`: `Display` only
- `String`: `Display` only

### Example Implementation: i32

```wado
impl Display for i32 {
    fn fmt(&self, f: &mut Formatter) {
        f.write_str(self.to_string());
    }
}

impl Binary for i32 {
    fn fmt(&self, f: &mut Formatter) {
        let value = *self;

        // Handle alternate form: add "0b" prefix
        if f.alternate() {
            f.write_str("0b");
        }

        if value == 0 {
            f.write_char('0');
            return;
        }

        // Calculate number of significant bits
        let bits = if value < 0 { 32 } else { 32 - builtin::i32_clz(value) };

        // Write bits from most significant to least significant
        for let mut i = bits - 1; i >= 0; i -= 1 {
            let bit = (value >> i) & 1;
            f.write_char(if bit == 1 { '1' } else { '0' });
        }
    }
}

impl LowerHex for i32 {
    fn fmt(&self, f: &mut Formatter) {
        let value = *self;

        // Handle alternate form: add "0x" prefix
        if f.alternate() {
            f.write_str("0x");
        }

        if value == 0 {
            f.write_char('0');
            return;
        }

        let digits = "0123456789abcdef";
        let nibbles = if value < 0 { 8 } else {
            (35 - builtin::i32_clz(value)) / 4
        };

        for let mut i = nibbles - 1; i >= 0; i -= 1 {
            let nibble = (value >> (i * 4)) & 0xF;
            f.write_char(digits.char_at(nibble));
        }
    }
}
```

### Example Implementation: f64

```wado
impl Display for f64 {
    fn fmt(&self, f: &mut Formatter) {
        // Use precision if specified
        if let Some(prec) = f.precision() {
            f.write_str(builtin::f64_to_fixed_string(*self, prec));
        } else {
            f.write_str(self.to_string());
        }
    }
}

impl LowerExp for f64 {
    fn fmt(&self, f: &mut Formatter) {
        let prec = f.precision();
        f.write_str(builtin::f64_to_exp_string(*self, prec, false));
    }
}

impl UpperExp for f64 {
    fn fmt(&self, f: &mut Formatter) {
        let prec = f.precision();
        f.write_str(builtin::f64_to_exp_string(*self, prec, true));
    }
}
```

### Custom Type Formatting

Users implement format traits for custom types:

```wado
struct Point { x: i32, y: i32 }

impl Display for Point {
    fn fmt(&self, f: &mut Formatter) {
        f.write_char('(');
        // Nested formatting respects outer precision/width? No.
        // Each interpolation gets its own Formatter.
        f.write_str(self.x.to_string());
        f.write_str(", ");
        f.write_str(self.y.to_string());
        f.write_char(')');
    }
}

impl LowerHex for Point {
    fn fmt(&self, f: &mut Formatter) {
        f.write_char('(');
        // For hex formatting of fields, create sub-formatter or use helpers
        if f.alternate() {
            f.write_str("0x");
        }
        f.write_str(format_hex(self.x));
        f.write_str(", ");
        if f.alternate() {
            f.write_str("0x");
        }
        f.write_str(format_hex(self.y));
        f.write_char(')');
    }
}

// Usage
let p = Point { x: 255, y: 16 };
println(`{p}`);      // "(255, 16)"
println(`{p:x}`);    // "(ff, 10)"
println(`{p:#x}`);   // "(0xff, 0x10)"
println(`{p:>20}`);  // "          (255, 16)"
```

### Padding and Alignment

The `Formatter::finish()` method applies padding based on `FormatSpec`:

1. Calculate content length from `buf`
2. If `width > content.len()`:
   - Calculate padding needed
   - Apply `fill` character with `align` direction
3. Return final string

```wado
impl Formatter {
    fn finish(&mut self) -> String {
        if let Some(width) = self.spec.width {
            let content_len = self.buf.len();
            if width > content_len {
                let padding = width - content_len;
                return match self.spec.align {
                    Alignment::Left => {
                        self.buf + repeat_char(self.spec.fill, padding)
                    },
                    Alignment::Right => {
                        repeat_char(self.spec.fill, padding) + self.buf
                    },
                    Alignment::Center => {
                        let left = padding / 2;
                        let right = padding - left;
                        repeat_char(self.spec.fill, left)
                            + self.buf
                            + repeat_char(self.spec.fill, right)
                    },
                };
            }
        }
        return self.buf;
    }
}
```

### Zero Padding for Numbers

Zero padding (`{x:08}`) is special:

- Implies right alignment
- Zero is inserted after sign/prefix but before digits

```wado
// {-42:08} → "-0000042" (not "000000-42")
// {42:#08x} → "0x00002a" (not "000x002a")
```

This requires coordination between the trait implementation and `finish()`. The trait writes the sign/prefix first, then the formatter tracks where zero-padding should be inserted.

## Consequences

### Positive

1. **Accurate formatting**: Precision is available during formatting, not post-processing
2. **Efficient**: Write directly to buffer, avoid intermediate strings
3. **Rust-compatible**: Familiar design for Rust developers
4. **Flexible**: Format options available to trait implementations
5. **Extensible**: Easy to add new format options or traits

### Negative

1. **Infrastructure complexity**: Requires `Formatter`, `FormatSpec`, `Alignment` types
2. **Learning curve**: More complex than simple `-> String` approach
3. **Implementation effort**: All primitive formatting needs updating

### Implementation Strategy

1. Add `Alignment` enum, `FormatSpec` struct, `Formatter` struct to `core:prelude`
2. Add format traits to `core:prelude/traits.wado`
3. Implement traits for primitives in `core:prelude/primitives.wado`
4. Update compiler to generate Formatter-based code for template strings
5. Add helper builtins for float formatting with precision

### Future Extensions

1. **`{:#?}`**: Pretty-print debug with indentation
2. **Dynamic width/precision**: `{value:{width}.{precision}}`
3. **Named arguments**: `{name:spec}` with named parameters
4. **Custom format specifiers**: User-defined specifier characters

## References

- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust std::fmt module](https://doc.rust-lang.org/std/fmt/index.html)
- [Rust Formatter struct](https://doc.rust-lang.org/std/fmt/struct.Formatter.html)

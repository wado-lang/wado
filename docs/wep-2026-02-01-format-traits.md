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

This WEP defines the trait system that backs these format specifiers.

## Decision

### Trait Hierarchy

Format specifiers are backed by traits in `core:prelude`. The `:?` specifier uses the compiler intrinsic `builtin::inspect()` and does not require a trait.

```wado
// Default display formatting - used by `{expr}`
trait Display {
    fn fmt(&self) -> String;
}

// Binary formatting - used by `{expr:b}`
trait Binary {
    fn fmt_binary(&self) -> String;
}

// Octal formatting - used by `{expr:o}`
trait Octal {
    fn fmt_octal(&self) -> String;
}

// Lowercase hex formatting - used by `{expr:x}`
trait LowerHex {
    fn fmt_lower_hex(&self) -> String;
}

// Uppercase hex formatting - used by `{expr:X}`
trait UpperHex {
    fn fmt_upper_hex(&self) -> String;
}

// Lowercase exponential formatting - used by `{expr:e}`
trait LowerExp {
    fn fmt_lower_exp(&self) -> String;
}

// Uppercase exponential formatting - used by `{expr:E}`
trait UpperExp {
    fn fmt_upper_exp(&self) -> String;
}
```

### Trait Method Naming

Unlike Rust's `std::fmt` traits which all use a generic `fn fmt(&self, f: &mut Formatter)` pattern with a shared `Formatter` type, Wado uses distinct method names for each trait:

| Trait      | Method            | Rationale                                |
| ---------- | ----------------- | ---------------------------------------- |
| `Display`  | `fmt`             | Primary formatting, short name           |
| `Binary`   | `fmt_binary`      | Explicit, avoids collision with Display  |
| `Octal`    | `fmt_octal`       | Explicit, avoids collision with Display  |
| `LowerHex` | `fmt_lower_hex`   | Explicit, avoids collision with Display  |
| `UpperHex` | `fmt_upper_hex`   | Explicit, avoids collision with Display  |
| `LowerExp` | `fmt_lower_exp`   | Explicit, avoids collision with Display  |
| `UpperExp` | `fmt_upper_exp`   | Explicit, avoids collision with Display  |

This design:
1. Avoids the need for a `Formatter` infrastructure
2. Allows types to implement multiple format traits without method name collisions
3. Keeps each trait simple and self-contained
4. Returns `String` directly, matching Wado's value semantics

### Format Resolution

When the compiler processes `{expr:spec}`:

1. **No specifier** (`{expr}`):
   - If `expr` is `String` → use directly
   - If `expr: Display` → call `expr.fmt()`
   - Otherwise → call `builtin::inspect(expr)`

2. **Debug specifier** (`{expr:?}`):
   - Always call `builtin::inspect(expr)` (compiler intrinsic)

3. **Other specifiers** (`{expr:b}`, `{expr:x}`, etc.):
   - Look up the corresponding trait (`Binary`, `LowerHex`, etc.)
   - If type implements the trait → call the trait method
   - Otherwise → **compile error**

### Primitive Implementations

Integer types (`i8`, `i16`, `i32`, `i64`, `u8`, `u16`, `u32`, `u64`) implement:
- `Display` (via existing `to_string()`)
- `Binary`
- `Octal`
- `LowerHex`
- `UpperHex`

Float types (`f32`, `f64`) implement:
- `Display` (via existing `to_string()`)
- `LowerExp`
- `UpperExp`

Other primitives:
- `bool`: `Display` only (`"true"` / `"false"`)
- `char`: `Display` only (single character string)
- `String`: `Display` (returns self)

### Example Implementations

```wado
impl Display for i32 {
    fn fmt(&self) -> String {
        return self.to_string();
    }
}

impl Binary for i32 {
    fn fmt_binary(&self) -> String {
        let value = *self;
        if value == 0 {
            return "0";
        }

        // Handle negative numbers as two's complement
        let bits = if value < 0 { 32 } else {
            // Count significant bits for positive numbers
            32 - builtin::i32_clz(value)
        };

        let mut result = String::with_capacity(bits);
        for let mut i = bits - 1; i >= 0; i -= 1 {
            let bit = (value >> i) & 1;
            result.append(if bit == 1 { "1" } else { "0" });
        }
        return result;
    }
}

impl LowerHex for i32 {
    fn fmt_lower_hex(&self) -> String {
        let value = *self;
        if value == 0 {
            return "0";
        }

        let digits = "0123456789abcdef";
        let nibbles = if value < 0 { 8 } else {
            (35 - builtin::i32_clz(value)) / 4  // ceil((32 - clz) / 4)
        };

        let mut result = String::with_capacity(nibbles);
        for let mut i = nibbles - 1; i >= 0; i -= 1 {
            let nibble = (value >> (i * 4)) & 0xF;
            result.append(digits.get_byte(nibble).to_string());
        }
        return result;
    }
}

impl LowerExp for f64 {
    fn fmt_lower_exp(&self) -> String {
        // Use bundled float-to-string with exponential mode
        return builtin::f64_to_exp_string(*self, false);
    }
}
```

### Format Parameters

Format parameters (width, precision, alignment, sign, alternate form) are handled by the compiler, not the traits. The compiler:

1. Calls the appropriate trait method to get the base string
2. Applies padding/alignment based on width and fill
3. Applies precision (for floats)
4. Adds sign prefix if specified
5. Adds alternate prefix (`0x`, `0b`, `0o`) if specified

This separation keeps traits simple while allowing rich formatting.

```wado
// Compiler desugaring for `{x:#08x}`:
// 1. let base = x.fmt_lower_hex();        // "2a"
// 2. let prefixed = "0x" + base;          // "0x2a"
// 3. let padded = pad_left(prefixed, 8, '0');  // "0x00002a"
```

### Custom Type Formatting

Users can implement format traits for custom types:

```wado
struct Point { x: i32, y: i32 }

impl Display for Point {
    fn fmt(&self) -> String {
        return `({self.x}, {self.y})`;
    }
}

impl LowerHex for Point {
    fn fmt_lower_hex(&self) -> String {
        return `({self.x:x}, {self.y:x})`;
    }
}

// Usage
let p = Point { x: 255, y: 16 };
println(`{p}`);     // "(255, 16)"
println(`{p:x}`);   // "(ff, 10)"
```

### Trait vs Method

The `Display` trait's `fmt()` method is separate from the existing `to_string()` methods on primitives:

| Method        | Purpose                       | Called by          |
| ------------- | ----------------------------- | ------------------ |
| `to_string()` | Direct conversion to String   | User code          |
| `fmt()`       | Display trait implementation  | Format machinery   |

For primitives, `fmt()` delegates to `to_string()`. This allows:
- Backward compatibility with existing `to_string()` calls
- Clear separation between direct conversion and formatting
- Future flexibility (e.g., `fmt()` could support locale-aware formatting)

## Consequences

### Positive

1. **Type-safe formatting**: Format specifiers that don't apply to a type cause compile errors
2. **Extensible**: Custom types can implement any format trait
3. **Simple traits**: Each trait has a single method returning String
4. **No Formatter complexity**: Avoids Rust's `Formatter` infrastructure
5. **Consistent with Wado's design**: Returns values, not writes to buffers

### Negative

1. **Separate methods per trait**: Types implementing multiple traits have multiple methods
   - **Mitigation**: Clear naming convention; each method is simple
2. **Format parameters handled by compiler**: Less flexibility for custom formatting logic
   - **Mitigation**: Base formatting is still customizable; parameters are standardized
3. **Memory allocation**: Each format call allocates a String
   - **Mitigation**: GC handles cleanup; most formatting is temporary

### Future Extensions

1. **Formatter with options**: Could add `FormatOptions` struct for precision/width if trait-level control is needed
2. **`#[derive(Display)]`**: Auto-derive Display using field names (requires macro/attribute support)
3. **Locale-aware formatting**: `Display` could optionally use locale settings

## References

- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust std::fmt traits](https://doc.rust-lang.org/std/fmt/index.html#formatting-traits)

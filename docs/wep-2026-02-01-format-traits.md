# WEP: Format Traits

## Context

Template strings in Wado support format specifiers: `` `${x:spec}` ``. As defined in [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md), Wado uses Rust-compatible format specifiers:

| Specifier | Description           | Example               |
| --------- | --------------------- | --------------------- |
| (none)    | Default display       | `${x}` → `"42"`       |
| `?`       | Debug/Inspect         | `${x:?}` → `"42"`     |
| `b`       | Binary integers       | `${x:b}` → `"101010"` |
| `o`       | Octal integers        | `${x:o}` → `"52"`     |
| `x`       | Lowercase hex         | `${x:x}` → `"2a"`     |
| `X`       | Uppercase hex         | `${x:X}` → `"2A"`     |
| `e`       | Lowercase exponential | `${x:e}` → `"4.2e1"`  |
| `E`       | Uppercase exponential | `${x:E}` → `"4.2E1"`  |

(`x` is `42` in each row.)

This WEP defines the trait system and Formatter infrastructure that backs these format specifiers.

## Decision

### Formatter Infrastructure

Format traits write to a `Formatter` object that holds format options and a reference to the output buffer. The `Formatter` does not own its buffer; it writes directly into the caller's `&mut String`. Format specification fields are embedded directly into `Formatter` to avoid an extra GC struct allocation.

```wado
/// Text alignment for padding
enum Alignment {
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
    alternate: bool,   // the `#` flag
    zero_pad: bool,
    width: i32,        // NO_WIDTH (-1) = not specified
    precision: i32,    // PRECISION_DEFAULT (-2) = not specified
    indent: i32,       // pretty-print depth, used by the alternate Inspect
    buf: &mut String,
}
```

The fields are read directly; there are no accessors.

`Formatter`'s methods split into writing, padding, and pretty-printing:

```wado
impl Formatter {
    fn new(buf: &mut String) -> Formatter with stores[buf];

    // Writing
    fn write_str(&mut self, s: &String);
    fn write_char(&mut self, c: char);
    fn write_char_n(&mut self, c: char, n: i32);
    fn write_from_memory(&mut self, ptr: i32, len: i32);

    // Padding. `pad` takes rendered content; `mark` / `apply_padding` bracket a
    // write straight into the buffer; `prepare_int_write` reserves the digit
    // area of an integer, having placed sign, prefix and padding itself.
    fn pad(&mut self, content: String);
    fn mark(&self) -> i32;
    fn apply_padding(&mut self, start_pos: i32);
    fn prepare_int_write(&mut self, is_negative: bool, prefix: String, digit_count: i32) -> i32;

    // Pretty-printing
    fn write_indent(&mut self);
    fn write_newline_indent(&mut self);
    fn open_brace(&mut self, open: String);
    fn close_brace(&mut self, close: String);

    // The cap a sequence Inspect applies: the explicit precision, else
    // DEFAULT_SEQ_LIMIT.
    fn resolved_seq_limit(&self) -> i32;
}
```

Width counts characters, so padding scans the rendered bytes rather than
subtracting offsets. `synthesis::template` builds `Formatter` literals directly
instead of calling `new`, so it repeats the sentinel constants; the e2e fixture
`template_formatter_sentinels.wado` fails if the two drift.

`String::push_display` (the `PushDisplay` trait) writes one `Display` value into
a `String` in place, skipping the temporary that a `` `${value}` `` template
would allocate — the hot path in the serde numeric encoders.

### Format Traits

All format traits follow the same pattern: write to a `Formatter`. Each method
is named after its trait, so a type implementing several of them declares no
overloads.

```wado
pub trait Display {
    fn fmt(&self, f: &mut Formatter);
}

pub trait Binary {
    fn fmt_binary(&self, f: &mut Formatter);
}

pub trait Octal {
    fn fmt_octal(&self, f: &mut Formatter);
}

pub trait LowerHex {
    fn fmt_lower_hex(&self, f: &mut Formatter);
}

pub trait UpperHex {
    fn fmt_upper_hex(&self, f: &mut Formatter);
}

pub trait LowerExp {
    fn fmt_lower_exp(&self, f: &mut Formatter);
}

pub trait UpperExp {
    fn fmt_upper_exp(&self, f: &mut Formatter);
}
```

### Debug Formatting

Both `:?` and `:#?` resolve to `Inspect`; the `#` sets `alternate`, and the implementation branches on it for the pretty-printed (indented multi-line) form. A type's `Inspect` comes from its shape over `ReflectStruct` / `ReflectVariant` / `ReflectEnum`, through a blanket impl in `core:prelude/traits` — one impl per reflection kind, each branching on `alternate` — so no per-type synthesis is needed; a type can write its own `impl` to override. See [Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

```wado
internal trait Inspect {
    fn inspect(&self, f: &mut Formatter);
}
```

`Inspect` is `internal`: it is the compiler's dispatch target, not an API to
name in a bound. A type can still write its own `impl` to override the derived
one.

### Display Derivation

`Display` comes from the type's own `impl Display`. The compiler provides one
for the two type kinds whose string form is unambiguous: a plain `enum` displays
its bare case name (`Red`, vs `Inspect`'s `Color::Red`), and a newtype inherits
its base type's `Display` transparently (`Meters = f64` renders `3.14`). Any
other type — a struct, variant, or generic container — needs a hand-written
`impl Display`; `${x}` on a type without one is a compile error (use `${x:?}`).
`${x:#}` runs the same `Display` with `alternate` set, which an implementation
is free to ignore — most do. See
[Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### The Alternate Flag

`#` sets `Formatter.alternate`; it selects no trait of its own. Each
implementation decides what the flag means, and one that has no alternate form
ignores it:

| Trait                   | Syntax   | Effect of `alternate`                  |
| ----------------------- | -------- | -------------------------------------- |
| `Display`               | `${:#}`  | Implementation-defined; most ignore it |
| `Inspect`               | `${:#?}` | Pretty-print with indentation          |
| `Binary`                | `${:#b}` | Add `0b` prefix                        |
| `Octal`                 | `${:#o}` | Add `0o` prefix                        |
| `LowerHex`              | `${:#x}` | Add `0x` prefix                        |
| `UpperHex`              | `${:#X}` | Add `0x` prefix, `A-F` digits          |
| `LowerExp` / `UpperExp` | `${:#e}` | None                                   |

A radix prefix costs nothing to carry: it is already an argument of the shared
`fmt_base`, which passes it on only when the flag is set. The template
synthesiser writes the flag as a literal into the `Formatter`, so
`optimize::param_spec` clones the callee on it and constant folding drops the
arm not taken — the same code a separate trait produced.

### Format Resolution

| Specifier | Resolution                                             |
| --------- | ------------------------------------------------------ |
| (none)    | `Display::fmt` (compile error if `T` has no `Display`) |
| `?`       | `Inspect::inspect`                                     |
| `#`       | `Display::fmt` with `alternate`                        |
| `#?`      | `Inspect::inspect` with `alternate`                    |
| `b`       | `Binary::fmt_binary`                                   |
| `o`       | `Octal::fmt_octal`                                     |
| `x`       | `LowerHex::fmt_lower_hex`                              |
| `X`       | `UpperHex::fmt_upper_hex`                              |
| `e`       | `LowerExp::fmt_lower_exp`                              |
| `E`       | `UpperExp::fmt_upper_exp`                              |
| `f`       | `Display::fmt` (`Display` already honours precision)   |

### Primitive Implementations

| Type           | Traits                                                                       |
| -------------- | ---------------------------------------------------------------------------- |
| Integer types  | `Display`, `Binary`, `Octal`, `LowerHex`, `UpperHex`, `LowerExp`, `UpperExp` |
| Float types    | `Display`, `LowerExp`, `UpperExp`                                            |
| `bool`, `char` | `Display`                                                                    |
| `String`       | `Display`                                                                    |

`Inspect` is implemented for every primitive too. Nothing needs a second impl
for `#`: the numeric traits read the flag inside the shared `fmt_base`, and the
rest ignore it.

### Zero Padding

Zero padding (`${x:08}`) inserts zeros after sign/prefix but before digits, and
wins over an explicit fill and alignment:

```
${-42:08}   → "-0000042"
${42:#08x}  → "0x00002a"
${-42:*<08} → "-0000042"
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
3. **Alternate costs a branch when it cannot be folded**: at `-O0` / `-O1` both
   arms stay in the one function, where separate traits would have let DCE drop
   the unused half

### Known gaps

- [ ] Dynamic width/precision (`${value:${width}.${precision}}`) — see
      [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md).

## References

- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust std::fmt module](https://doc.rust-lang.org/std/fmt/index.html)

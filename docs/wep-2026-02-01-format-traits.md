# WEP: Format Traits

## Context

[WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
defines what `${x:spec}` accepts. This WEP defines what it dispatches to: the
trait per specifier, the `Formatter` they write through, and which types have
which impl.

## Decision

### Formatter

A format trait writes into a `Formatter`, which carries the parsed spec and a
reference to the caller's buffer. It does not own the buffer, and the spec
fields are embedded rather than boxed, so formatting one value allocates no GC
struct of its own.

```wado
enum Alignment {
    Left,
    Center,
    Right,
}

struct Formatter {
    fill: char,
    align: Alignment,
    sign_plus: bool,   // `+`
    alternate: bool,   // `#`
    zero_pad: bool,    // `0`
    width: i32,        // NO_WIDTH (-1) when the spec set none
    precision: i32,    // PRECISION_DEFAULT (-2) when the spec set none
    indent: i32,       // nesting depth of the alternate `Inspect`
    buf: &mut String,
}
```

The fields are public and read directly; there are no accessors.

```wado
impl Formatter {
    fn new(buf: &mut String) -> Formatter with stores[buf];

    // Writing
    fn write_str(&mut self, s: &String);
    fn write_char(&mut self, c: char);
    fn write_char_n(&mut self, c: char, n: i32);
    fn write_from_memory(&mut self, ptr: i32, len: i32);

    // Padding
    fn pad(&mut self, content: String);
    fn mark(&self) -> i32;
    fn apply_padding(&mut self, start_pos: i32);
    fn prepare_int_write(&mut self, is_negative: bool, alt_prefix: String, digit_count: i32) -> i32;

    // Alternate `Inspect`
    fn write_indent(&mut self);
    fn write_newline_indent(&mut self);
    fn open_brace(&mut self, open: String);
    fn close_brace(&mut self, close: String);

    fn resolved_seq_limit(&self) -> i32;
}
```

Three padding paths exist because content reaches the buffer three ways:

| Method                   | Input                                | Used by                     |
| ------------------------ | ------------------------------------ | --------------------------- |
| `pad`                    | a rendered `String`                  | `String`, `char`, `bool`    |
| `mark` / `apply_padding` | content already written at an offset | floats, composite renderers |
| `prepare_int_write`      | a digit count, to fill backwards     | integers                    |

`prepare_int_write` places the sign, the radix prefix and the padding itself,
then returns the offset of the reserved digit area. It writes `alt_prefix` only
when `alternate` is set, so a caller passes `"0x"` unconditionally rather than
choosing between it and an empty string — an empty `String` would leave a value
copy behind that no pass removes.

Width counts characters, so `pad` and `apply_padding` count them rather than
subtracting byte offsets.

`synthesis::template` writes `Formatter` literals directly instead of calling
`new`, so it repeats the sentinel constants; the e2e fixture
`template_formatter_sentinels.wado` fails if the two copies drift.

`String::push_display` (the `PushDisplay` trait) writes one `Display` value into
a `String` in place, skipping the temporary a `` `${value}` `` template would
allocate. It is the hot path in the serde numeric encoders.

### The traits

Each method is named after its trait, so a type implementing several declares
no overloads.

```wado
pub trait Display   { fn fmt(&self, f: &mut Formatter); }
pub trait Binary    { fn fmt_binary(&self, f: &mut Formatter); }
pub trait Octal     { fn fmt_octal(&self, f: &mut Formatter); }
pub trait LowerHex  { fn fmt_lower_hex(&self, f: &mut Formatter); }
pub trait UpperHex  { fn fmt_upper_hex(&self, f: &mut Formatter); }
pub trait LowerExp  { fn fmt_lower_exp(&self, f: &mut Formatter); }
pub trait UpperExp  { fn fmt_upper_exp(&self, f: &mut Formatter); }

internal trait Inspect { fn inspect(&self, f: &mut Formatter); }
```

`Inspect` is `internal` — it is the compiler's dispatch target for `?`, and the
stdlib's own impls are what a program normally reaches. A `T: Inspect` bound is
accepted and always holds, since the trait is total over every type, and a type
may write an `impl Inspect` to override the derived one. See
[WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

### Resolution

| Specifier | Resolves to                                            |
| --------- | ------------------------------------------------------ |
| (none)    | `Display::fmt` — compile error if `T` has no `Display` |
| `f`       | `Display::fmt` — `Display` already honours precision   |
| `?`       | `Inspect::inspect`                                     |
| `b`       | `Binary::fmt_binary`                                   |
| `o`       | `Octal::fmt_octal`                                     |
| `x`       | `LowerHex::fmt_lower_hex`                              |
| `X`       | `UpperHex::fmt_upper_hex`                              |
| `e`       | `LowerExp::fmt_lower_exp`                              |
| `E`       | `UpperExp::fmt_upper_exp`                              |

`#` changes no row: it sets `Formatter.alternate`, and the implementation the
row already names reads it.

### The alternate flag

| Trait                   | Syntax   | Effect of `alternate`                  |
| ----------------------- | -------- | -------------------------------------- |
| `Display`               | `${:#}`  | Implementation-defined; most ignore it |
| `Inspect`               | `${:#?}` | Pretty-print with indentation          |
| `Binary`                | `${:#b}` | `0b` prefix                            |
| `Octal`                 | `${:#o}` | `0o` prefix                            |
| `LowerHex`              | `${:#x}` | `0x` prefix                            |
| `UpperHex`              | `${:#X}` | `0x` prefix, `A-F` digits              |
| `LowerExp` / `UpperExp` | `${:#e}` | None                                   |

A flag rather than a second trait per specifier costs one branch on a field the
template synthesiser writes as a literal, so `optimize::param_spec` clones the
callee on it and constant folding drops the arm not taken. Below `-O2` no such
clone happens and both arms remain.

Where the two forms differ by more than a constant, the alternate body is
outlined into its own function that the branch calls — as `write_seq_inspect`
calls `write_seq_inspect_alt` — so the plain path stays the size it was and
DCE drops the alternate one when nothing asks for `#`.

### Debug formatting

`:?` and `:#?` both resolve to `Inspect`, which derives from the type's shape
through one blanket impl per reflection kind in `core:prelude/traits`:

| Bound            | Renders                                           |
| ---------------- | ------------------------------------------------- |
| `ReflectStruct`  | `Name { field: value, … }`, secret fields as `..` |
| `ReflectVariant` | `Type::Case(payload)`, a unit case bare           |
| `ReflectEnum`    | `Type::Case`                                      |
| `ReflectFlags`   | set bits joined by `\|`, else `Type::none()`      |

Each branches on `alternate` where the two forms differ, calling out to the
outlined alternate body, so a struct, variant, enum or flags type needs no
synthesised impl of its own. What reflection does
not cover — newtypes, resources, and the `fn(..)` dispatch stubs — the compiler
still emits per type. See
[WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md) and
[WEP: Inspect](./wep-2026-02-21-inspect-debug-output.md).

### Display derivation

`Display` comes from the type's own `impl Display`. The compiler writes one for
the two kinds whose string form is unambiguous:

- a plain `enum` renders its bare case name (`Red`, against `Inspect`'s
  `Color::Red`);
- a newtype inherits its base type's `Display` transparently (`Meters = f64`
  renders `3.14`).

Every other type — struct, variant, generic container — needs a hand-written
impl; `${x}` without one is a compile error naming `${x:?}` as the debug form.
So `T: Display` certifies a real string representation, which is what
`String::push_display` takes.

### Primitive implementations

| Type           | Traits                                                                       |
| -------------- | ---------------------------------------------------------------------------- |
| Integer types  | `Display`, `Binary`, `Octal`, `LowerHex`, `UpperHex`, `LowerExp`, `UpperExp` |
| Float types    | `Display`, `LowerExp`, `UpperExp`                                            |
| `bool`, `char` | `Display`                                                                    |
| `String`       | `Display`                                                                    |

`Inspect` is implemented for every primitive. None of them needs a second impl
for `#`: the numeric traits read the flag inside the shared `fmt_base`, and the
rest ignore it.

## Consequences

The `Formatter` reaches every implementation, so precision, width and the
alternate flag are available wherever a value is rendered — including inside a
container, which shares the caller's `Formatter` with its elements.

`Alignment` and `Formatter` are types the stdlib must carry, and each primitive
needs one impl per format trait it supports; there is no blanket that covers
them.

Reading `alternate` at run time rather than dispatching on it costs a branch
wherever it cannot be folded — at `-O0` and `-O1` both arms stay in the one
function, where a separate trait per form would have let DCE drop the half a
program does not use.

### Known gaps

- [ ] Dynamic width/precision (`${value:${width}.${precision}}`) — see
      [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md).

## References

- [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [WEP: Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust `std::fmt`](https://doc.rust-lang.org/std/fmt/)

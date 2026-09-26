# Literals

## Primitive Literals

### Boolean Literals

```wado
let active = true;
let disabled = false;
```

### Null Literal

The `null` keyword is equivalent to `None` and represents the absence of a value:

```wado
let missing: Option<i32> = null;            // Same as None
let also_missing = Option::<i32>::None;

// Both are equivalent
assert missing == also_missing;
```

Note: `null` is a language keyword, while `None` is a case of the prelude's `Option`. Bare `None` needs an expected type to say which `Option` it belongs to.

A `null` takes the `Option` its context expects. With no context it has the type `Option<!>`, the `Option` with no `Some`. A `null` is never any type but an `Option`, so `let x: i32 = null` is a type error.

`Option<!>` is a value of every `Option<T>`. Like an integer literal, a `null` answers a type parameter last, so a sibling argument decides which `Option` it is. A `let` settles its type at once, as it does an integer literal's:

```wado
fn pair<T>(a: T, b: T) -> T { return a; }

let p = pair(null, Option::Some(1));    // Option<i32>
let x = null;                           // Option<!>
// x = Option::Some(1);                 // Error: expected Option<!>
// if let Some(v) = x { }               // Error: unreachable, no Option<!> is a Some
```

`Option<!>` is also what a type converts from to accept `null` where an `Option` is not expected, which is how `core:value::Value` takes JSON's `null` in a literal:

```wado
impl From<Option<!>> for Value {
    fn from(value: Option<!>) -> Value {
        return Value::Null;
    }
}

let doc: Value = { name: "Alice", nickname: null };
```

### Character Literals

Character literals use single quotes and represent a Unicode scalar value. `char` is a distinct type with Unicode semantics, not an integer, just as `String` is not `List<u8>`:

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
```

See [Escape Sequences](#escape-sequences) for the supported escapes.

```wado
let a = '\u0041';         // 'A' (BMP)
let smiley = '\u{1F600}'; // '😀' (non-BMP)
```

#### char Casting and Conversion

`char` can be cast to any integer type to extract the Unicode scalar value (possibly truncated for smaller types):

```wado
let c = 'A';
let code = c as i32;    // 65
let ucode = c as u32;   // 65
let byte = c as u8;     // 65 (truncated to low byte)
```

`u8 as char` is allowed because all `u8` values (0..255) are valid Unicode scalar values:

```wado
let byte: u8 = 65;
let c = byte as char;  // 'A'
```

All other integer-to-char casts are prohibited because not all values are valid Unicode scalar values (surrogates `0xD800..0xDFFF` and values `> 0x10FFFF` are invalid):

```wado
let x: i32 = 65;
let c = x as char;  // compile error

let y: i8 = 65;
let c = y as char;  // compile error (i8 can be negative)
```

Use checked conversion functions instead:

```wado
let c = char::from_u32(65 as u32);  // Option<char>: Some('A')
let c = char::from_i32(65);         // Option<char>: Some('A')
```

See [`core:prelude`](./stdlib-core-prelude.md) for the full `char` API.

Casting `char` to non-integer types is a compile error:

```wado
let c = 'A';
let f = c as f64;     // compile error: char can only be cast to integer types
let s = c as String;  // compile error: the two types share no representation
```

### Integer Literals

```wado
let decimal = 42;
let negative = -17;
let with_separator = 1_000_000;    // Underscores for readability
let binary = 0b1010_1100;          // Binary
let octal = 0o755;                 // Octal
let hex = 0xFF_AA_BB;              // Hexadecimal
```

#### Type coercion

When the target type is known from context (type annotation or function argument), integer literals coerce to any compatible integer type, including `i128`/`u128`:

```wado
let byte: i8 = 127;
let long: i64 = 9_223_372_036_854_775_807;
let unsigned: u32 = 4_294_967_295;
let big: u128 = 1_000_000_000_000;
fn foo(n: i64) { ... }
foo(100);  // literal coerced to i64
```

#### Compile-time range checking

The compiler rejects literal coercions whose value falls outside the target type's range. All literal bases (decimal, hex `0x`, octal `0o`, binary `0b`) use strict numeric range: the value must lie within `[MIN, MAX]` for signed types or `[0, MAX]` for unsigned types.

To reinterpret a bit pattern as a signed integer, use an explicit `as` cast.

```wado
let a: i8 = 127;                  // OK: max i8
let b: i8 = 128;                  // compile error: literal out of range for `i8`: 128
let c: i8 = -128;                 // OK: min i8
let d: u32 = -1;                  // compile error: literal out of range for `u32`: -1

let e: i8 = 0xFF;                 // compile error: literal out of range for `i8`: 0xFF
let f: i8 = 0xFF as i8;           // OK: explicit bit-pattern reinterpretation (value: -1)
let g: i32 = 0xFFFF_FFFF;         // compile error: literal out of range for `i32`: 0xFFFF_FFFF
let h: i32 = 0xFFFF_FFFF as i32;  // OK: explicit bit-pattern reinterpretation (value: -1)
let i: u32 = 0x1_0000_0000;       // compile error: literal out of range for `u32`: 0x1_0000_0000
```

A float has no bit pattern for `as` to reinterpret. An integer literal cast to
`f32` or `f64` converts by value, as the annotated form does, and the same
range check applies.

```wado
let m = 340282350000000000000000000000000000000 as f32;  // OK: f32::MAX
let n = 400000000000000000000000000000000000000 as f32;  // compile error: literal out of range for `f32`
```

A literal that nothing coerces falls back to `i32`, and the same range check applies there. `-NUM` is checked as one literal, so it reaches the signed minimum.

```wado
let j = 2147483647;               // OK: max i32
let k = 4294967296;               // compile error: literal out of range for `i32`: 4294967296
let l = -2147483648;              // OK: min i32
let m = -2147483649;              // compile error: literal out of range for `i32`: -2147483649
```

Type conversion (via `as`):

```wado
let byte: i8 = 127 as i8;
let long: i64 = 9_223_372_036_854_775_807 as i64;
let unsigned: u32 = 4_294_967_295 as u32;
```

### Floating-Point Literals

```wado
let pi = 3.14159;
let with_separator = 1_000_000.5;
let scientific = 6.022e23;         // 6.022 × 10²³
let negative_exp = 1.6e-19;        // 1.6 × 10⁻¹⁹
let explicit_positive = 2.5e+10;
```

#### Type coercion

Floating-point literals coerce to `f32`, `f64`, `f16` or `bf16` when the target type is known:

```wado
let single: f32 = 3.14;
let double: f64 = 3.14159265358979;
let half: f16 = 0.5;
let weights: List<bf16> = [0.5, -1.25, 3.0];
```

A literal is rounded once, from its decimal text to the nearest value of its
type, ties to even. One that rounds past the type's largest finite value is a
compile error, as an integer literal past its type's range is:

```wado
let x: f16 = 65520.0;             // compile error: literal out of range for `f16`: 65520.0
let y: f32 = 1e39;                // compile error: literal out of range for `f32`: 1e39
```

Type conversion (via `as`):

```wado
let single: f32 = 3.14 as f32;
let double: f64 = 3.14159265358979 as f64;
```

### String Literals

String literals create `String` values.

Regular strings use double quotes:

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
```

Byte strings use a `b` prefix and create a constant `ByteList` (the
first-class byte-buffer newtype over `List<u8>`):

```wado
let magic = b"\x89PNG\r\n";             // Type: ByteList, value [137, 80, 78, 71, 13, 10]
let raw: List<u8> = b"\x89PNG\r\n";     // Also OK: newtype literal coercion to the base
```

The content must be ASCII; each `\xNN` escape (two hex digits) or source
character contributes one byte, and the standard escapes (`\n`, `\t`, `\\`,
`\"`, `\0`, `\r`, `\'`) are also accepted. Unicode escapes (`\u{...}` /
`\uHHHH`) are rejected, since a Unicode escape denotes a scalar, not a byte. Use
`\xNN` for a raw byte. (`#include_bytes("path")` produces the same `ByteList` from a file.)
The default type is `ByteList`, but newtype literal coercion lets it flow into a
`List<u8>` context (or any type whose base is `List<u8>`) with no cast.

Byte literals are the single-byte analog: `b'x'` is one `u8`.

```wado
let a = b'A';              // u8, 65
let hi = b'\xff';          // u8, 255
let n: i32 = b'A';         // 65 — coerces like an integer literal
```

A byte literal is an integer literal defaulting to `u8`, so it coerces like any
integer literal (its value is always `0..=255`). Its content follows the same
one-byte rule as a byte string (`\xNN` for `0x80..=0xFF`; no `\u`); use a char
literal `'…'` for a Unicode scalar.

### Escape Sequences

Escape sequences are shared between character, string and template literals,
except where the table names one:

| Escape   | Character                   |
| -------- | --------------------------- |
| `\'`     | Single quote                |
| `\"`     | Double quote                |
| `` \` `` | Backtick (template only)    |
| `\\`     | Backslash                   |
| `\/`     | Forward slash               |
| `\b`     | Backspace                   |
| `\f`     | Form feed                   |
| `\n`     | Newline                     |
| `\r`     | Carriage return             |
| `\t`     | Tab                         |
| `\0`     | Null                        |
| `\$`     | Dollar sign (template only) |
| `\{`     | Left brace (template only)  |
| `\}`     | Right brace (template only) |
| `\uHHHH` | Unicode BMP (4 hex digits)  |
| `\u{H+}` | Unicode full range          |

A bare `{` or `}` in a template string needs no escape, as
[Template Strings](#template-strings) states, but `\{` and `\}` are accepted.
Use `\$` to write a literal `$` before a `{`: `` `\${x}` `` renders the text
`${x}`.

For characters outside BMP (U+10000 and above), use either:

```wado
"\uD83D\uDE00"   // Surrogate pair
"\u{1F600}"      // Variable-length escape
"😀"             // Direct Unicode character
```

Multiline strings are supported in both regular and template strings. Literal newlines are preserved:

```wado
// Regular multiline string
let poem = "Roses are red,
Violets are blue,
Wado is great,
And so are you!";

// Multiline template string
let name = "Alice";
let message = `Dear ${name},

Welcome to Wado!

Best regards`;
```

### Template Strings

A template string is written in backticks and has the type `String`. `${expr}`
interpolates the value of `expr`, and everything else is literal text. A bare
`{` or `}` is literal, so JSON-like content needs no escaping:

```wado
let name = "Alice";
let greeting = `Hello, ${name}!`;   // "Hello, Alice!"
let json = `{"key": "${name}"}`;    // {"key": "Alice"}

let pi = 3.14159;
let formatted = `Pi: ${pi:.2}`;     // "Pi: 3.14"
let p = Point { x: 10, y: 20 };
let debug = `${p:?}`;               // "Point { x: 10, y: 20 }"
```

#### Interpolation

An interpolation is `${expr}` or `${expr:spec}`. The expression may be any
expression, not only a name:

```wado
`${x + 1}`
`${x * 2:x}`
`${p.x + p.y}`
`${arr.len()}`
```

The specifier starts at the first `:` outside parentheses, brackets and braces.
A `::` is always a path separator, so `${foo::bar}` and `${foo::<T>}` hold only
an expression, while `${foo:x}` has the specifier `x`. The interpolation ends at
the `}` that closes its `${`. A `}` inside a string, a char, a nested template
or a comment does not close it.

Whitespace around the expression and around the specifier is ignored, so
`${ x : 5 }` reads as `${x:5}`.

#### Format Specifiers

```text
interpolation := '${' expression [ ':' spec ] '}'
spec          := [[fill] align] ['+'] ['#'] ['0'] [width] ['.' precision] [type]
align         := '<' | '^' | '>'
type          := 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | 'f' | '?'
width         := digit+
precision     := digit+
```

Every part of `spec` is optional, but `spec` itself is not: a `:` must be
followed by one. `width` and `precision` must fit in an `i32`.

A character is a fill only when an alignment follows it. The fill may be any
character except `'`, `"`, `` ` ``, `{`, `}` and a `/` that opens a comment,
since the interpolation reads those as structure. Only `+` is a sign, so in
`${42:-<4}` the `-` is the fill, and it renders `42--`.

The grammar is closed. Anything it does not accept is a compile error, so a
printf-style `${x:08d}` is rejected rather than rendered as `${x:08}`:

| Input              | Error                                             |
| ------------------ | ------------------------------------------------- |
| `${x:}`            | empty format specifier                            |
| `${x:d}`           | unknown format specifier `d`                      |
| `${x:5x1}`         | unexpected `1` after the format specifier         |
| `${x:.}`           | expected digits after `.` in format specifier     |
| `${x:99999999999}` | format specifier width `99999999999` is too large |

#### Format Types

The type character selects the trait the value renders through. A value whose
type does not implement that trait is a compile error.

| Type   | Trait      | Applies to             | Example                   |
| ------ | ---------- | ---------------------- | ------------------------- |
| (none) | `Display`  | types with an impl     | `${42}` → `42`            |
| `f`    | `Display`  | types with an impl     | `${3.14159:.2f}` → `3.14` |
| `?`    | `Inspect`  | every type             | `${"a":?}` → `"a"`        |
| `b`    | `Binary`   | integers               | `${42:b}` → `101010`      |
| `o`    | `Octal`    | integers               | `${42:o}` → `52`          |
| `x`    | `LowerHex` | integers               | `${42:x}` → `2a`          |
| `X`    | `UpperHex` | integers               | `${42:X}` → `2A`          |
| `e`    | `LowerExp` | integers, `f32`, `f64` | `${1200:e}` → `1.2e3`     |
| `E`    | `UpperExp` | integers, `f32`, `f64` | `${1200:E}` → `1.2E3`     |

`f` selects no trait of its own. `Display` already honours precision, so
`${x:.2f}` and `${x:.2}` render the same. Writing `f` only marks the format as a
float format for the reader. There is no adaptive `g` and no pointer `p`.

Which types implement `Display` is stated in
[Format Traits](./spec-traits.md#format-traits).

`b`, `o`, `x` and `X` render a negative signed integer as the two's complement
bit pattern of its own width: `${-1 as i32:x}` renders `ffffffff`.

`e` and `E` write one digit before the point. On an integer the mantissa drops
its trailing zeros (`${1200:e}` renders `1.2e3`). With a precision it carries
exactly that many decimal places, rounded half to even (`${1250:.1e}` renders
`1.2e3`, `${1350:.1e}` renders `1.4e3`). A carry moves the exponent up a decade:
`${99:.0e}` renders `1e2`.

#### Width, Fill and Alignment

`width` is a minimum length, counted in characters, not bytes or display
columns. Padding uses the fill character, a space by default, and every type
aligns right by default. Centering puts the odd character of padding on the
right. A multi-byte fill pads by whole characters.

```wado
`${42:5}`        // "   42"
`${42:<5}`       // "42   "
`${42:^5}`       // " 42  "
`${42:_>5}`      // "___42"
`${"あい":>6}`    // "    あい"  two characters, six bytes
`${42:€>8}`      // "€€€€€€42"
```

#### Sign and Zero Padding

`+` writes a sign on a non-negative number. Without it only a negative number
carries one.

`0` is a flag, not a width digit, so `${x:0.2f}` is zero padding plus a
precision. Zeros go after the sign and after any radix prefix, and the flag wins
over an explicit fill and alignment:

```wado
`${42:+}`         // "+42"
`${42:05}`        // "00042"
`${-42:08}`       // "-0000042"
`${-42:*<08}`     // "-0000042"
`${42:#08x}`      // "0x00002a"
`${-1200.0:012e}` // "-0000001.2e3"
```

#### Alternate Form

`#` asks for the alternate form. It selects no trait: it sets a flag the
implementation reads, or ignores.

| Type     | Effect of `#`        | Example    | Output     |
| -------- | -------------------- | ---------- | ---------- |
| `x`, `X` | `0x` prefix          | `${42:#x}` | `0x2a`     |
| `b`      | `0b` prefix          | `${42:#b}` | `0b101010` |
| `o`      | `0o` prefix          | `${42:#o}` | `0o52`     |
| `?`      | indented, multi-line | `${p:#?}`  | multi-line |
| (none)   | up to the `Display`  | `${42:#}`  | `42`       |
| `e`, `E` | none                 | `${42:#e}` | `4.2e1`    |

`${x:#X}` prefixes `0x`, not `0X`. The flag chooses the prefix, and the type
character chooses the case of the digits.

A hand-written `Display` may branch on the flag. `core:temporal`'s `Instant`
renders whole seconds plainly and milliseconds under `#`. Every primitive
ignores it.

#### Precision

`precision` means something different for each kind of value:

| Value                          | Meaning                      | Example                               |
| ------------------------------ | ---------------------------- | ------------------------------------- |
| Float                          | decimal places               | `${3.14159:.2}` → `3.14`              |
| Integer or float under `e`/`E` | mantissa decimal places      | `${12345:.2e}` → `1.23e4`             |
| Integer otherwise              | ignored                      | `${42:.2}` → `42`                     |
| `String`, `StrSlice`           | maximum length in characters | `${"hello world":.5}` → `hello`       |
| `List`, `Array`, `Slice`       | maximum number of elements   | `${[1, 2, 3, 4, 5]:.3}` → `[1, 2, 3]` |

`Display` truncates silently. `Inspect` marks the cut: a string gets `...` after
its closing quote, and a sequence gets `, ...` in place of the dropped elements.
The marker does not count toward the precision.

```wado
let s = "hello world";
`${s:.5}`        // hello
`${s:.5?}`       // "hello"...
`${s:.20}`       // hello world

let a: List<i32> = [1, 2, 3, 4, 5];
`${a:.3}`        // [1, 2, 3]
`${a:.3?}`       // [1, 2, 3, ...]
```

A container renders its elements with the same spec it was given, so precision
and width reach every element. A tuple or struct never caps its own arity, but
its string and sequence fields honour the precision, and `${a:6?}` pads each
element rather than the whole list.

#### The Formatter

Every format trait writes into a `Formatter`, which carries the parsed spec and
the string being built. Both types are in the prelude:

```wado
pub enum Alignment { Left, Center, Right }

pub struct Formatter {
    pub fill: char,          // ' ' when the spec sets none
    pub align: Alignment,    // Right when the spec sets none
    pub sign_plus: bool,     // `+`
    pub alternate: bool,     // `#`
    pub zero_pad: bool,      // `0`
    pub width: i32,          // Formatter::NO_WIDTH (-1) when the spec sets none
    pub precision: i32,      // Formatter::PRECISION_DEFAULT (-2) when the spec sets none
    pub indent: i32,         // nesting depth of the alternate Inspect
    pub buf: &mut String,    // the output, which the Formatter does not own
}

pub trait Display with ()  { fn fmt(&self, f: &mut Formatter); }
pub trait Inspect with ()  { fn inspect(&self, f: &mut Formatter); }
pub trait Binary with ()   { fn fmt_binary(&self, f: &mut Formatter); }
pub trait Octal with ()    { fn fmt_octal(&self, f: &mut Formatter); }
pub trait LowerHex with () { fn fmt_lower_hex(&self, f: &mut Formatter); }
pub trait UpperHex with () { fn fmt_upper_hex(&self, f: &mut Formatter); }
pub trait LowerExp with () { fn fmt_lower_exp(&self, f: &mut Formatter); }
pub trait UpperExp with () { fn fmt_upper_exp(&self, f: &mut Formatter); }
```

Each method is named after its trait, so a type implementing several declares
no overloads. Every format trait is `with ()`, so rendering a value performs no
effect.

An interpolation builds one `Formatter` from its spec and calls the one method
its type character selects. It writes nothing around that call. Padding, sign
and truncation belong to the implementation, which reads the public fields and
writes through `f`. An implementation that writes its text with `f.write_str`
ignores the spec; one that hands its rendered text to `f.pad` honours width,
fill, alignment and zero padding.

```wado
struct Celsius { degrees: i32 }

impl Display for Celsius {
    fn fmt(&self, f: &mut Formatter) {
        f.pad(`${self.degrees}°C`);
    }
}

`${Celsius { degrees: 21 }:>8}`   // "    21°C"
```

`Formatter::new(buf)` builds a formatter with the default spec, and
`String::push_display(&value)` renders one `Display` value into a string in
place. The full method set is in [`core:prelude`](./stdlib-core-prelude.md).

`precision` holds three kinds of value:

| Value                     | Meaning                                                                        |
| ------------------------- | ------------------------------------------------------------------------------ |
| `>= 0`                    | the precision the spec wrote                                                   |
| `PRECISION_DEFAULT` (-2)  | none written; an `Inspect` of a string or sequence caps at `DEFAULT_SEQ_LIMIT` |
| `PRECISION_INFINITE` (-1) | uncapped; an `Inspect` of a string or sequence renders all of it               |

`.N` cannot write a negative number, so `PRECISION_INFINITE` is reachable only
by building a `Formatter` directly.

#### Display Output

The prelude's `Display` impls render as follows. Plain enums and newtypes render
as [Format Traits](./spec-traits.md#format-traits) states.

- An integer renders in decimal.
- A float renders the shortest digits that read back as the same value, in
  positional notation and with no trailing `.0`: `${5.0}` renders `5`,
  `${3.14}` renders `3.14`. The special values render `NaN`, `inf` and `-inf`,
  and negative zero renders `-0`. `f16` and `bf16` render as their `f32` value.
- `bool` renders `true` or `false`, `char` the character itself, and `String`
  and `StrSlice` their text. `()` renders `()`.
- `List`, `Array` and `Slice` render `[a, b, c]`, each element in its `Inspect`
  form.
- A tuple renders `[a, b]`, each element in its `Display` form. A tuple has
  `Display` only when every element does.
- A range renders `start..<end` or `start..=end`.
- A closure renders as its `Inspect` form, so `${f}` writes the signature and
  `${f:#}` the source.

#### Inspect Output

`Inspect` is the debug form behind `${x:?}` and `${x:#?}`. Every type has it,
as [Derivation Policy](./spec-traits.md#derivation-policy) states. The prelude
implements it for the primitives, `String`, references, sequences, tuples and
the standard collections. A struct, variant, enum, flags or newtype derives it
from its shape, and a resource and a closure have a built-in one.

The output follows Wado's literal syntax where it has one:

| Type                     | Output                                    | Example                                    |
| ------------------------ | ----------------------------------------- | ------------------------------------------ |
| Integer                  | decimal                                   | `42`, `-7`                                 |
| `f32`, `f64`             | shortest round-trip digits, see below     | `3.14`, `5.0`, `inf`, `-0.0`               |
| `f16`, `bf16`            | exponent form of the `f32` value          | `1e0`                                      |
| `bool`                   | `true` / `false`                          | `true`                                     |
| `char`                   | quoted, escaped                           | `'A'`, `'\n'`                              |
| `String`, `StrSlice`     | quoted, escaped                           | `"say \"hi\""`                             |
| `()`                     | `()`                                      | `()`                                       |
| `v128`                   | its 16 bytes in hex, byte lane 0 first    | `v128(0x000102030405060708090a0b0c0d0e0f)` |
| Struct                   | `Name { field: value, ... }`              | `Point { x: 10, y: 20 }`                   |
| Struct, no fields        | `Name {}`                                 | `Empty {}`                                 |
| Tuple                    | `[elem, ...]`                             | `[1, "a", true]`                           |
| `List`, `Array`, `Slice` | `[elem, ...]`                             | `[1, 2, 3]`                                |
| `TreeMap<K, V>`          | `{key: value, ...}`                       | `{"a": 1, "b": 2}`                         |
| `TreeSet<T>`             | `{elem, ...}`                             | `{10, 20, 30}`                             |
| Enum                     | `Type::Case`                              | `Color::Red`                               |
| Variant                  | `Type::Case(payload)`, a unit case bare   | `Shape::Circle(5.0)`, `Option::None`       |
| Flags                    | set members joined by `\|`                | `Perms::Read \| Perms::Write`              |
| Flags, none set          | `Type::none()`                            | `Perms::none()`                            |
| Newtype                  | base value, then `as Name`                | `1.5 as Meters`                            |
| `&T`, `&mut T`           | `&` or `&mut`, then the referent          | `&42`, `&mut Point { x: 1, y: 2 }`         |
| Closure                  | its signature                             | `\|i32, i32\| -> i32`                      |
| Affine resource          | `Name#0x` and the handle in lowercase hex | `Widget#0xff`                              |

Rules the table does not carry:

- A float renders its shortest round-trip digits. A value with no fractional
  part keeps `.0`, and a value whose decimal exponent is below -4 or at least 16
  renders in exponent form (`1e300`). The special values render `NaN`, `inf`,
  `-inf` and `-0.0`.
- A string escapes `"`, `\`, newline, carriage return and tab as `\"`, `\\`,
  `\n`, `\r` and `\t`. Any other control character (below `0x20`, or `0x7F`)
  renders as `\u{HEX}` in lowercase hex. Every other character, non-ASCII
  included, renders as itself. A `char` escapes `'`, `\`, newline, carriage
  return and tab the same way.
- A struct writes its fields in declaration order and never its type arguments,
  so `Box<i32>` renders `Box { value: 42 }`. A
  [`#[secret]`](./spec-attributes.md#secret) field is left out and `..` appended:
  `User { name: "Alice", .. }`, or `Name { .. }` when every field is secret.
- An enum, variant or flags case is always qualified by its type, as its
  construction is spelled. `Option` and `Result` are ordinary variants and get
  no short form.
- A newtype writes its base, then `as` and its own name, as the cast that
  builds it reads. A chain writes one link per newtype: `7 as A as B`.
- An [unrestricted resource](./spec-components.md#resource-linearity) renders
  as the resource its handle's class names, whatever the static type says, with
  the class and object index: `Element { type_id: 1, object_id: 7 }`. A class no
  resource in the [inheritance tree](./spec-components.md#resource-inheritance)
  owns keeps the static type's name. An `f64` that encodes no class and object,
  or the handle of a resource that declares no classes, renders as the `f64`:
  `Node { handle: 1.5 }`. Inspecting a handle never traps.
- A closure under `#` renders its own source, reprinted with each binary
  operation parenthesised: `|x: i32| (x + 1)`. A captured variable appears by
  name. A function used as a value renders as a closure that forwards to it:
  `|x: i32| double(x)`.

Under `#`, each composite writes one element, field or entry per line, indented
two spaces per level, each followed by a comma. An empty one stays on one line
(`[]`, `{}`), and a variant's payload goes on its own line:

```wado
let arr: List<i32> = [1, 2, 3];
`${arr:#?}`                        // "[\n  1,\n  2,\n  3,\n]"
`${Option::Some(42):#?}`           // "Option::Some(\n  42,\n)"
`${Point { x: 1, y: 2 }:#?}`       // "Point {\n  x: 1,\n  y: 2,\n}"
```

#### Inspect Truncation

With no precision in the spec, `Inspect` of a string or a sequence caps it at
`Formatter::DEFAULT_SEQ_LIMIT`, 256 characters or elements, and marks the cut as
an explicit precision does, which keeps debug output readable. An explicit
precision replaces the cap. `Display` never applies it.

The cap covers `String`, `StrSlice`, `List`, `Array` and `Slice`. A sequence
that shows no element before the cut renders `[...]`, and the `#` form puts
`...` on a line of its own. A tuple, a struct, a `TreeMap` and a `TreeSet` are
never capped.

```wado
let long = "a".repeat(300);
`${long}`.len()      // 300
`${long:?}`.len()    // 261: two quotes, 256 characters, and "..."
```

Rationale: [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md),
[WEP: Format Traits](./wep-2026-02-01-format-traits.md),
[WEP: Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md).

### Tuple Literals

Bracket syntax `[...]` creates tuple values by default. This aligns with TypeScript conventions and JSON interoperability.

```wado
let pair = [1, "hello"];              // Type: [i32, String]
let triple = [42, "answer", true];    // Type: [i32, String, bool]
let single = [42];                    // Type: [i32] (1-tuple)
let empty_tuple: [] = [];             // Empty tuple (distinct from unit ())
let trailing = [1, 2, 3,];            // Trailing comma allowed
```

#### Tuple Types

Tuple types use bracket syntax `[T1, T2, ...]`.

```wado
let point: [i32, i32] = [10, 20];
let record: [String, i32, bool] = ["Alice", 30, true];
```

#### Tuple Element Access

Tuple elements are accessed by constant index using dot notation or bracket notation:

```wado
let t = [10, "hello", true];
let x = t.0;      // 10 - dot notation
let y = t[1];     // "hello" - bracket notation
let z = t.2;      // true

// Variable index is not allowed (compile error)
let i = 1;
let w = t[i];     // Error: tuple index must be a constant integer
```

#### Unit vs Empty Tuple

The unit type `()` and empty tuple `[]` are distinct:

```wado
let unit: () = ();    // Unit type/value
let empty: [] = [];   // Empty tuple (rarely used)
```

They take separate `impl`s, so a method defined on one is not found on the other.

`[]` cannot cross a component boundary. It has no Component Model representation:
a `tuple` carries at least one type, and `()` is the type that carries none. An
export naming one is rejected at compile time.

#### The Never Type `!`

The never type `!` is the bottom type: it is a subtype of every type. An expression of type `!` never returns, because it always diverges (traps). `panic()` and `unreachable()` both return `!`.

Because `!` is assignable to any type, an expression of type `!` may appear in any value position without a type mismatch:

```wado
// In a match arm: the None branch panics, so the match has type i32
let opt: Option<i32> = Option::<i32>::Some(5);
let x = match opt {
    Some(v) => v,
    None => panic("unexpected none"),
};

// In a let binding with explicit type annotation
let y: i32 = panic("unreachable");

// In a binary expression: execution diverges before the addition
let z: i32 = panic("boom") + 1;
```

The `!` type can be written explicitly as a return type:

```wado
fn fail(msg: String) -> ! {
    panic(msg);
}
```

### List Literals

A bracket literal coerces to a `List` where the target type is known, as a
function parameter or a type annotation makes it. Elsewhere it is a tuple, and
`as List<T>` converts it.

```wado
let t = [1, 2, 3];                      // Tuple [i32, i32, i32]: no context
let numbers = [1, 2, 3] as List<i32>;   // explicit conversion

fn takes_list(a: List<i32>) { ... }
takes_list([1, 2, 3]);                  // implicit coercion to the parameter type
let explicit: List<i32> = [1, 2, 3];    // implicit coercion to the annotation
```

Rationale: [WEP: Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md).

#### List Constructors

```wado
let arr = List::<i32>::with_capacity(10);     // empty list with room for 10 elements
let bools = List::<bool>::filled(100, true);  // list of 100 elements, all true
```

#### List Operations

```wado
let mut arr: List<i32> = [1, 2, 3];

// Index access (read)
let first = arr[0];  // 1

// Index assignment (write)
arr[0] = 100;        // Requires a `let mut` binding
arr[1] = 200;

// List methods
arr.push(4);         // Add element to end
let len = arr.len(); // Get length
```

#### Index Assignment Rules

- Requires the list binding to be declared with `let mut`
- Index must be within bounds (runtime check, traps if out of bounds)
- Works with lists of any element type

#### Sorting

Every sort is stable and O(n log n) in the worst case:

| Method        | Mutates? | Comparator                          |
| ------------- | -------- | ----------------------------------- |
| `sort()`      | Yes      | `Ord::cmp` (requires `T: Ord`)      |
| `sort_by()`   | Yes      | Custom `fn mut(&T, &T) -> Ordering` |
| `sorted()`    | No       | `Ord::cmp` (requires `T: Ord`)      |
| `sorted_by()` | No       | Custom `fn mut(&T, &T) -> Ordering` |

On a float, `Ord` is the IEEE 754 total order rather than the order `<` gives,
so a NaN still has a place in a sorted list. See
[Ord - Ordering](./spec-traits.md#ord---ordering).

```wado
let mut nums: List<i32> = [5, 3, 8, 1];
nums.sort();                             // in-place ascending

let orig: List<i32> = [5, 3, 8, 1];
let asc = orig.sorted();                // returns a new sorted list
```

### Collection Literal Coercion

Sequence literals `[e0, e1, ...]` and key-value literals `{ k: v, ... }` can be
coerced to any collection type through `From`. Coercing one materializes an
`Array`, which the target's `From<Array<…>>` impl builds from:

| Literal         | Materializes    | Impl the target writes                            |
| --------------- | --------------- | ------------------------------------------------- |
| `[e0, e1, ...]` | `Array<E>`      | `From<Array<T>> for List<T>`                      |
| `{ k: v, ... }` | `Array<[K, V]>` | `From<Array<[String, V]>> for TreeMap<String, V>` |

This applies only where a coercion runs. The tuple and struct readings keep
their priority: with no target type `[1, 2, 3]` is still the tuple
`[i32, i32, i32]` (see [List Literals](#list-literals)), and `{ … }` against a
nominal struct with matching fields is still a struct literal.

A key-value literal is an array of pairs, so `[["a", 1]]` builds the same map
`{ a: 1 }` does. `Array<T>` itself needs no impl, since the array the coercion
materializes is already the result.

#### Usage

```wado
let arr: List<i32> = [1, 2, 3];

use { TreeMap } from "core:collections";
let map: TreeMap<String, i32> = { width: 1920, height: 1080 };
```

Making a user type literal-constructible is one ordinary impl:

```wado
impl<T> From<Array<T>> for MyVec<T> {
    fn from(elements: Array<T>) -> MyVec<T> { ... }
}
```

#### Impl Selection

The target's `From<Array<X>>` impls decide which literal form it accepts:

- `{ … }` considers only the impls whose `X` is a two-element tuple. A target
  with none is a compile error: it cannot be built from a key-value literal.
- `[ … ]` considers every `From<Array<X>>` impl. Where several match, the one
  whose `X` is not a two-element tuple wins.
- Several candidates left for one form are an ambiguity error at the literal.
  Writing `T::from(…)` out resolves it by ordinary overload resolution, with a
  turbofish where the target is generic (`List::<i32>::from(…)`).

`core:value::Value` accepts both forms, and each literal reads as JSON reads it:

| Literal      | `List<i32>`    | `TreeMap<String, i32>` | `Value`                        |
| ------------ | -------------- | ---------------------- | ------------------------------ |
| `[1, 2]`     | accepted       | error: not pairs       | `Value::List`                  |
| `{a: 1}`     | error: no impl | accepted               | `Value::Object`                |
| `[["a", 1]]` | error          | accepted, as a map     | `Value::List` of `Value::List` |

The selected impl's `X` types every element, and every key and value. Where the
target's type arguments are still open, as in a generic callee's parameter, the
elements decide them.

Every key a key-value literal writes is a field name, so the selected impl's
key type must accept a `String`. A target that builds from another key type is
a compile error at the literal.

A newtype target is built through the first type on its newtype chain that has
an impl, and the result is cast to the newtype:

```wado
type Scores = List<i32>;
let s: Scores = [90, 85];   // List::from, then `as Scores`
```

#### Implicit Conversion

A literal is implicitly converted to its target type through `From`. No other
expression is implicitly converted. A literal here is a number, string, char,
`bool`, `null` or byte literal, or a `[ … ]` or `{ … }` literal. A template
string, a variable and a call are not literals.

```wado
let v: List<Value> = [1, "x"];   // OK: every element is a literal
let v: List<Value> = [a, b];     // Error: write [Value::from(a), Value::from(b)]
let w: List<i64> = [x];          // Error: no implicit widening of `x: i32`
```

Literal typing runs first, so `42` against `i64` is an `i64`, not an
`i64::from`. `From` applies only where literal typing cannot reach the target:
against `Value`, the `1` in `{ n: 1 }` takes its default type `i32` and then
converts through `Value::from`. An element that reaches no conversion is
reported with the rule that refused it: the element is not a literal, or the
slot's type has no `From` for the element's type.

#### `..base` Spread

A key-value literal may carry `..base` members, which merge through
`LiteralSpread`:

```wado
pub trait LiteralSpread with () {
    fn spread_literal(&mut self, base: Self);
}
```

The members apply in source order and the last write wins. Each subexpression
is evaluated once, in source order. A spread may stand anywhere in the literal,
and there may be several. Each `base` must have the literal's target type.

```wado
let base: TreeMap<String, i32> = { a: 1, b: 2 };
let more: TreeMap<String, i32> = { a: 9, d: 4 };
let m: TreeMap<String, i32> = { ..base, ..more, c: 3 };   // a: 9, b: 2, c: 3, d: 4
```

These are compile errors:

- a target type without a `LiteralSpread` impl;
- `{ ..base }` with no other member, which only copies `base`;
- the same key written twice as an explicit member.

A sequence literal cannot carry a spread: `[..xs, 4]` is a
[tuple spread](./spec-functions.md#value-spread). The struct forms of `..base`
are in [Struct Construction](./spec-types.md#struct-construction) and
[Anonymous Structs](./spec-types.md#composition).

Rationale: [WEP: Literal Coercion as `From<Array<…>>`](./wep-2026-08-24-literal-from-array.md),
[WEP: Literal Spread (`..base`)](./wep-2026-07-03-literal-spread.md).

## Compile-Time Location Literals

Compile-time location literals provide source location information at compile time. They use the `#` prefix to clearly signal compile-time evaluation.

| Literal                  | Type       | Value                                              |
| ------------------------ | ---------- | -------------------------------------------------- |
| `#file`                  | `String`   | Current source file path                           |
| `#line`                  | `i32`      | Current line number (1-indexed)                    |
| `#function`              | `String`   | Name of the enclosing function                     |
| `#data`                  | `String`   | `__DATA__` section content (compile error if none) |
| `#include_str("path")`   | `String`   | External file content as string                    |
| `#include_bytes("path")` | `ByteList` | External file content as bytes                     |

```wado
fn example() {
    println(`Error at ${#file}:${#line}`);
    println(`In function: ${#function}`);
}
```

### `#data`

Returns the raw text content of the `__DATA__` section as a `String`. This is useful for programs that need to access embedded metadata at runtime (e.g., configuration, test fixtures, embedded documents). Using `#data` in a file that has no `__DATA__` section is a compile error.

```wado
export fn run() with Stdout {
    let config = #data;  // contains the __DATA__ section text
    println(config);
}

__DATA__
{"key": "value"}
```

### `#include_str` and `#include_bytes`

`#include_str("path")` reads an external file at compile time and returns its content as a `String`. The file must be valid UTF-8; otherwise, a compile error is raised. `#include_bytes("path")` returns the raw bytes as `ByteList` without UTF-8 validation.

The argument is a parenthesized string literal; any other expression is a compile error. A local path starts with `./` or `../` and resolves relative to the source file containing the expression, as [Module Path Validation](./spec-modules.md#module-path-validation) states for every path literal. A file that does not exist is a compile error.

```wado
let template = #include_str("./templates/header.html");
let icon: ByteList = #include_bytes("./assets/logo.png");
```

The file is read once, at compile time, and its content is a constant. Changing the file after compilation does not change the compiled program. The content is inserted as data and never expanded, so a file may include itself: `#include_str` of its own path yields its own source text.

Rationale: [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

### `#function` Format

Returns the name without type arguments or signature:

| Context                 | `#function` value            |
| ----------------------- | ---------------------------- |
| Free function           | `my_function`                |
| Method                  | `Point::distance`            |
| Method of `Box<String>` | `Box::name`                  |
| Closure                 | `parent_function::{closure}` |

### Call-site evaluation in default arguments

A [default argument](./spec-functions.md#default-arguments) resolves its names where it is declared, as [Where a Default Resolves](./spec-functions.md#where-a-default-resolves) states. `#file`, `#line` and `#function` are the exception: they evaluate at the call site, so a defaulted location parameter reports the caller.

```wado
pub fn log(msg: String, file: String = #file, line: i32 = #line) { /* ... */ }

log("started"); // file/line report this call, not where `log` is defined
```

Where a default's own call fills a default in turn (`fn outer(x = loc())`), every one of these literals reports the outermost call (`outer(...)`).

`#data`, `#include_str` and `#include_bytes` in a default read the file that wrote the default. A struct field default is not redirected either: its location literals report the file that declares the struct.

## Object Literals

Object literal syntax supports unquoted keys and shorthand properties.

For struct initialization syntax, see the [Structs](./spec-types.md#structs) section.

### TreeMap (Insertion-Order Map)

For associative arrays, use `TreeMap` from `core:collections`:

```wado
use { TreeMap } from "core:collections";

let mut map = TreeMap::<String, i32>::new();
map["x"] = 10;                   // insert or overwrite
map["y"] = 20;

let v = map["x"];                // panics if key not found
let opt = map.get("x");          // returns Option<V>
map.try_insert("x", 99);         // inserts only if absent; reports whether it did

// Keys preserve insertion order
let keys = map.keys();  // an iterator over the keys, in insertion order

// Functional-update spread: seed from a base map, then override/add keys
let m2: TreeMap<String, i32> = { ..map, "x": 99, "w": 40 };
```

The rules for a spread in a key-value literal are in
[`..base` Spread](#base-spread).

### Access Methods

```wado
// Struct: dot notation
user.name

// TreeMap: bracket notation or methods
map["key"]        // panics if key not found
map.get("key")    // returns Option<V>
```

## Known gaps

### Template Strings

- Width and precision are literal digits, so neither can be computed at run
  time.
- A spec part that means nothing for its value is dropped rather than rejected,
  against the closed grammar: a precision on an integer, `+` on a `String`, `#`
  under `e` or `E`.
- The `0` flag and the default alignment do not tell a number from any other
  value. `${true:08}` renders `0000true`, and text aligns right by default where
  Rust aligns it left.
- Nothing caps nesting depth the way `DEFAULT_SEQ_LIMIT` caps length, so
  inspecting a deeply recursive value runs until the stack is exhausted.

### Collection Literals

- A key is always a field name. A computed key such as `{ [Color::Red]: 1 }`
  cannot be written, so a map whose key type is not `String` has no key-value
  literal.

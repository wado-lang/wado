# Literals

This chapter covers the values a program writes directly: booleans, `null`,
characters, numbers, strings and byte strings, template strings and their
format specifiers, tuple, list and object literals, and the compile-time
literals that start with `#`. The types these values have are in
[Types](./spec-types.md).

## Primitive Literals

### Boolean Literals

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let active = true;
let disabled = false;
assert active && !disabled;
```

### Null Literal

`null` is the `None` of an `Option`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let missing: Option<i32> = null;            // Same as None
let also_missing = Option::<i32>::None;

// Both are equivalent
assert missing == also_missing;
```

`null` is a keyword, while `None` is a case of the prelude's `Option`. A bare
`None` needs an expected type to say which `Option` it belongs to.

A `null` takes the `Option` its context expects. It also takes a newtype over an
`Option`, as any literal coerces to a newtype whose base takes it: with
`type Opt<T> = Option<T>`, `let a: Opt<i64> = null` holds, and so does
`a == null`. With no context its type is `Option<!>`, the `Option` with no
`Some`. A `null` is never any other type, so `let x: i32 = null` is a type
error.

`Option<!>` is a value of every `Option<T>`. Like an integer literal, a `null` answers a type parameter last, so a sibling argument decides which `Option` it is. A `let` settles its type at once, as it does an integer literal's:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
fn pair<T>(a: T, b: T) -> T { return a; }

test {
    let p = pair(null, Option::Some(1));    // Option<i32>
    let x = null;                           // Option<!>
    assert p == null && x matches { None };
}
```

So `x` takes no `Some`:

<!-- {"fixture":"spec_literals_null_assign.wado"} -->

```wado
let mut x = null;
x = Option::Some(1);                 // Error: expected Option<!>
```

And a pattern that asks for one never matches:

<!-- {"fixture":"spec_literals_null_some.wado"} -->

```wado
let x = null;
if let Some(v) = x { }               // Error: unreachable, no Option<!> is a Some
```

`Option<!>` is also what a type converts from to accept `null` where an `Option` is not expected, which is how `core:value::Value` takes JSON's `null` in a literal:

<!-- {"fixture":"spec_literals_null_value.wado"} -->

```wado
impl From<Option<!>> for Value {
    fn from(value: Option<!>) -> Value {
        return Value::Null;
    }
}

test {
    let doc: Value = { name: "Alice", nickname: null };
    assert doc matches { Object(fields) && fields["nickname"] matches { Null } };
}
```

### Character Literals

Character literals use single quotes and represent a Unicode scalar value. `char` is a distinct type with Unicode semantics, not an integer, just as `String` is not `List<u8>`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let letter = 'A';
let digit = '9';
let unicode = '\u0041';  // Unicode escape (same as 'A')
let emoji = '😀';        // Direct Unicode character
let newline = '\n';
assert unicode == letter && digit as u32 == 57;
assert emoji as u32 == 0x1F600 && newline as u32 == 10;
```

A `\u` escape writes a scalar by its code point, one of the
[escape sequences](#escape-sequences) a character literal accepts:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let a = '\u0041';         // 'A' (BMP)
let smiley = '\u{1F600}'; // '😀' (non-BMP)
assert a == 'A' && smiley == '😀';
```

### Integer Literals

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let decimal = 42;
let negative = -17;
let with_separator = 1_000_000;    // Underscores for readability
let binary = 0b1010_1100;          // Binary
let octal = 0o755;                 // Octal
let hex = 0xFF_AA_BB;              // Hexadecimal
assert decimal + negative == 25 && with_separator == 1000000;
assert binary == 172 && octal == 493 && hex == 16755387;
```

#### Type coercion

When the target type is known from context (type annotation or function argument), integer literals coerce to any compatible integer type, including `i128`/`u128`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
fn foo(n: i64) -> i64 { return n; }

test {
    let byte: i8 = 127;
    let long: i64 = 9_223_372_036_854_775_807;
    let unsigned: u32 = 4_294_967_295;
    let big: u128 = 1_000_000_000_000;
    assert foo(100) == 100;  // literal coerced to i64
    assert long == i64::MAX && unsigned == u32::MAX;
}
```

A cast types the literal as an annotation does (see
[Numeric Casts](./spec-expressions.md#numeric-casts)):

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let byte: i8 = 127 as i8;
let long: i64 = 9_223_372_036_854_775_807 as i64;
let unsigned: u32 = 4_294_967_295 as u32;
assert byte == i8::MAX && long == i64::MAX && unsigned == u32::MAX;
```

An expression built from literals alone, through unary `-` and `~` and the
arithmetic, bitwise and shift operators, takes its type from its context the
same way. As an operand, it takes the type of the other operand:

<!-- {"fixture":"literal_coercion_through_operators.wado"} -->

```wado
let t: u32 = builtin::black_box(0xFFFF_FFFF as u32);
assert t & ~7 == 0xFFFF_FFF8;
assert t & (1 << 31) == 0x8000_0000;
```

#### Compile-time range checking

A literal's value must lie in its type's range: `[MIN, MAX]` for a signed type,
`[0, MAX]` for an unsigned one. This holds in every base, so a hex, octal or
binary literal is a number, not a bit pattern. A bit pattern is reinterpreted
by casting a value of the unsigned type:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let a: i8 = 127;                  // OK: max i8
let c: i8 = -128;                 // OK: min i8
let h = (0xFFFF_FFFF as u32) as i32;  // OK: a u32 value transmuted to i32
assert a == i8::MAX && c == i8::MIN && h == -1;
```

A value outside the range is an error in any base, annotated or cast:

<!-- {"fixture":"spec_literals_int_range.wado"} -->

```wado
let b: i8 = 128;                  // compile error: literal out of range for `i8`: 128
let d: u32 = -1;                  // compile error: literal out of range for `u32`: -1
let e: i8 = 0xFF;                 // compile error: literal out of range for `i8`: 0xFF
let g: i32 = 0xFFFF_FFFF;         // compile error: literal out of range for `i32`: 0xFFFF_FFFF
let i: u32 = 0x1_0000_0000;       // compile error: literal out of range for `u32`: 0x1_0000_0000
```

An integer literal cast to a float type converts by value, as the annotated
form does, and the same range check applies.

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let m = 340282350000000000000000000000000000000 as f32;  // OK
assert m == f32::MAX;
```

<!-- {"fixture":"spec_literals_int_to_f32_range.wado"} -->

```wado
let n = 400000000000000000000000000000000000000 as f32;  // compile error: literal out of range for `f32`
```

A literal that nothing coerces falls back to `i32`, and the same range check applies there. `-NUM` is checked as one literal, so it reaches the signed minimum.

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let j = 2147483647;               // OK: max i32
let l = -2147483648;              // OK: min i32
assert j == i32::MAX && l == i32::MIN;
```

<!-- {"fixture":"spec_literals_i32_fallback_range.wado"} -->

```wado
let k = 4294967296;               // compile error: literal out of range for `i32`: 4294967296
let m = -2147483649;              // compile error: literal out of range for `i32`: -2147483649
```

### Floating-Point Literals

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let pi = 3.14159;
let with_separator = 1_000_000.5;
let scientific = 6.022e23;         // 6.022 × 10²³
let negative_exp = 1.6e-19;        // 1.6 × 10⁻¹⁹
let explicit_positive = 2.5e+10;
assert with_separator == 1000000.5 && explicit_positive == 25000000000.0;
assert scientific == 602200000000000000000000.0 && negative_exp < 1e-18;
```

#### Type coercion

Floating-point literals coerce to `f32`, `f64`, `f16` or `bf16` when the target type is known:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let single: f32 = 3.14;
let double: f64 = 3.14159265358979;
let half: f16 = 0.5;
let weights: List<bf16> = [0.5, -1.25, 3.0];
assert f32::from(half) == 0.5 && f32::from(weights[1]) == -1.25;
```

A cast to `f32` or `f64` types the literal the same way:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let single: f32 = 3.14 as f32;
let double: f64 = 3.14159265358979 as f64;
assert single as f64 != double;
```

A literal is rounded once, from its decimal text to the nearest value of its
type, ties to even. One that rounds past the type's largest finite value is a
compile error, as an integer literal past its type's range is:

<!-- {"fixture":"spec_literals_float_range.wado"} -->

```wado
let x: f16 = 65520.0;             // compile error: literal out of range for `f16`: 65520.0
let y: f32 = 1e39;                // compile error: literal out of range for `f32`: 1e39
```

### Numeric Suffixes

A numeric literal may end with a type suffix, written after an `_`: one of
`i8`, `i16`, `i32`, `i64`, `i128`, `u8`, `u16`, `u32`, `u64`, `u128`, `f16`,
`bf16`, `f32` and `f64`. The literal has the type its suffix names. An integer
literal with a float suffix is a float.

<!-- {"fixture":"numeric_literal_suffix.wado"} -->

```wado
let byte = 255_u8;
assert byte as i32 == 255;
let low = -128_i8;
assert low as i32 == -128;
let half = 1.5_f32;
assert half as f64 == 1.5;
assert 1_f64 == 1.0;
```

The `_` is required. Letters written directly after a literal are its suffix,
so a suffix without the `_` or one that names no type is an error:

<!-- {"fixture":"error_numeric_literal_suffix_spelling.wado"} -->

```wado
let a = 255u8;               // compile error: write `255_u8`: a suffix follows an `_`
```

<!-- {"fixture":"error_numeric_literal_suffix_unknown.wado"} -->

```wado
let a = 255_u9;              // compile error: unknown numeric suffix `u9`
```

A hex, octal or binary literal takes an integer suffix only. In a hex literal
`b` and `f` are digits, so a float suffix there is read as more digits:

<!-- {"fixture":"numeric_literal_suffix.wado"} -->

```wado
assert 0x1_f32 == 0x1F32;
```

<!-- {"fixture":"error_numeric_literal_suffix_float_radix.wado"} -->

```wado
let a = 0b1_f32;             // compile error: a float suffix needs a decimal literal
```

A suffix starts with a letter, so a decimal digit an octal or binary literal
lacks is an invalid digit rather than the start of one:

<!-- {"fixture":"error_numeric_literal_radix_digit.wado"} -->

```wado
let a = 0b102;               // compile error: invalid digit `2` in a binary literal
```

A suffix is a type annotation. The literal is checked against its type as
`let x: T = literal` checks it, with a leading `-` read as part of the literal.
The context does not retype a suffixed literal, and a literal pattern with a
suffix must have the type of the scrutinee:

<!-- {"fixture":"error_numeric_literal_suffix_type.wado"} -->

```wado
let a = 300_u8;              // compile error: literal out of range for `u8`: 300
let b = -1_u8;               // compile error: literal out of range for `u8`: -1
let c = 1e40_f32;            // compile error: literal out of range for `f32`: 1e40
let d = 1.5_i32;             // compile error: cannot use float literal '1.5' as integer
let h = 2.5_i128;            // compile error: cannot use float literal '2.5' as integer
let i = 1e-5_u128;           // compile error: cannot use float literal '1e-5' as integer
let j: i128 = 3.5;           // compile error: cannot use float literal '3.5' as integer
let e: i64 = 1_i32;          // compile error: expected 'i64', found 'i32'
let f: Meters = 1.0_f64;     // compile error: expected 'Meters', found 'f64'
let x: u8 = 0;
let g = match x {
    1_i32 => 1,              // compile error: pattern mismatch: expected 'i32', found 'u8'
    _ => 0,
};
```

An operand beside a suffixed literal takes the literal's type, as it would
beside any typed value:

<!-- {"fixture":"numeric_literal_suffix.wado"} -->

```wado
let wide = 1_i64 << 40;      // an i64: `1 << 40` alone is an i32
assert wide == 1_099_511_627_776;
```

#### The `literal_cast` Lint

A cast whose operand is an unsuffixed literal, bare or negated, warns where the
suffix of its target types the literal as the cast does, and the warning gives
the suffixed spelling. A float literal cast to an integer type converts, so it
does not warn, and neither does a cast that no suffix can write.
`#[allow(literal_cast)]` waives the lint (see
[`#[allow(...)]`](./spec-attributes.md#allow)).

<!-- {"fixture":"literal_cast_lint.wado", "assert": false} -->

```wado
let a = 255 as u8;           // warns: write `255_u8` for `255 as u8`
let b = -128 as i8;          // warns: write `-128_i8` for `-128 as i8`
let c = 1.5 as f32;          // warns: write `1.5_f32` for `1.5 as f32`
let d = 0xFF as u64;         // warns: write `0xFF_u64` for `0xFF as u64`
let e = 0x10 as f64;         // no warning: a hex literal takes no float suffix
let f = 1.5 as i32;          // no warning: this cast converts, to 1
let g = 4 as Meters;         // no warning: a newtype has no suffix
```

## String Literals

A string literal is written in double quotes and has the type `String`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let name = "Alice";           // Type: String
let path = "path/to/file.txt";
let escaped = "Line 1\nLine 2\tTabbed";
assert escaped.lines().count() == 2 && escaped.len() == 20;
```

A string or template string may span lines. Each newline in the source is a
newline in the value:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
test "multiline" {
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
    assert poem.lines().count() == 4;
    assert message == "Dear Alice,\n\nWelcome to Wado!\n\nBest regards";
}
```

Byte strings use a `b` prefix and create a constant `ByteList`, the byte-buffer
newtype over `List<u8>`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let magic = b"\x89PNG\r\n";             // Type: ByteList
let raw: List<u8> = b"\x89PNG\r\n";     // Also OK: newtype literal coercion to the base
assert raw == [137, 80, 78, 71, 13, 10] && magic as List<u8> == raw;
```

Each source character and each escape contributes one byte. The source
characters must be ASCII. A `\xNN` escape (two hex digits) writes any byte, and
the standard escapes `\n`, `\t`, `\\`, `\"`, `\0`, `\r` and `\'` are accepted
too. A Unicode escape (`\u{...}` or `\uHHHH`) is an error, since it denotes a
scalar, not a byte. [`#include_bytes`](#include_str-and-include_bytes) builds
the same `ByteList` from a file.

Like any literal, a byte string coerces to the base of its newtype, so it flows
into a `List<u8>` context, or one whose base is `List<u8>`, with no cast.

Byte literals are the single-byte analog: `b'x'` is one `u8`.

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
let a = b'A';              // u8
let hi = b'\xff';          // u8
let n: i32 = b'A';         // coerces like an integer literal
assert a == 65 && hi == 255 && n == 65;
```

A byte literal is an integer literal whose default type is `u8`, so it coerces
as any integer literal does. Its value is always in `0..=255`. Its content
follows a byte string's rule: one ASCII character or escape, with `\xNN` for
`0x80..=0xFF` and no `\u`. A char literal `'…'` writes a Unicode scalar.

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

A bare `{` or `}` in a template string needs no escape
([Template Strings](#template-strings)), but `\{` and `\}` are accepted. `\$`
writes a literal `$` before a `{`: `` `\${x}` `` renders the text `${x}`.

A character outside the Basic Multilingual Plane (U+10000 and above) is written
as a surrogate pair of `\uHHHH` escapes, or as one `\u{H+}`:

<!-- {"fixture":"spec_literals_primitives.wado"} -->

```wado
assert "\uD83D\uDE00" == "😀";   // Surrogate pair
assert "\u{1F600}" == "😀";      // Variable-length escape
```

## Template Strings

A template string is written in backticks and has the type `String`. `${expr}`
interpolates the value of `expr`, and everything else is literal text. A bare
`{` or `}` is literal, so JSON-like content needs no escaping:

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
let name = "Alice";
let greeting = `Hello, ${name}!`;
let json = `{"key": "${name}"}`;
assert greeting == "Hello, Alice!" && json == "{\"key\": \"Alice\"}";

let pi = 3.14159;
let formatted = `Pi: ${pi:.2}`;
let p = Point { x: 10, y: 20 };
let debug = `${p:?}`;
assert formatted == "Pi: 3.14" && debug == "Point { x: 10, y: 20 }";
```

### Interpolation

An interpolation is `${expr}` or `${expr:spec}`. The expression may be any
expression, not only a name:

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
assert `${x + 1}` == "42";
assert `${x * 2:x}` == "52";
assert `${p.x + p.y}` == "30";
assert `${arr.len()}` == "3";
```

The specifier starts at the first `:` outside parentheses, brackets and braces.
A `::` is always a path separator, so `${foo::bar}` and `${foo::<T>}` hold only
an expression, while `${foo:x}` has the specifier `x`. The interpolation ends at
the `}` that closes its `${`. A `}` inside a string, a char, a nested template
or a comment does not close it.

Whitespace around the expression and around the specifier is ignored, so
`${ x : 5 }` reads as `${x:5}`.

### Format Specifiers

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

### Format Types

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

### Width, Fill and Alignment

`width` is a minimum length, counted in characters, not bytes or display
columns. Padding uses the fill character, a space by default, and every type
aligns right by default. Centering puts the odd character of padding on the
right. A multi-byte fill pads by whole characters.

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
assert `${42:5}` == "   42";
assert `${42:<5}` == "42   ";
assert `${42:^5}` == " 42  ";
assert `${42:_>5}` == "___42";
assert `${"あい":>6}` == "    あい";  // two characters, six bytes
assert `${42:€>8}` == "€€€€€€42";
```

### Sign and Zero Padding

`+` writes a sign on a non-negative number. Without it only a negative number
carries one.

`0` is a flag, not a width digit, so `${x:0.2f}` is zero padding plus a
precision. Zeros go after the sign and after any radix prefix, and the flag wins
over an explicit fill and alignment:

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
assert `${42:+}` == "+42";
assert `${42:05}` == "00042";
assert `${-42:08}` == "-0000042";
assert `${-42:*<08}` == "-0000042";
assert `${42:#08x}` == "0x00002a";
assert `${-1200.0:012e}` == "-0000001.2e3";
```

### Alternate Form

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

A hand-written `Display` may branch on the flag. Every primitive's `Display`
ignores it.

### Precision

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

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
let s = "hello world";
assert `${s:.5}` == "hello";
assert `${s:.5?}` == "\"hello\"...";
assert `${s:.20}` == "hello world";

let a: List<i32> = [1, 2, 3, 4, 5];
assert `${a:.3}` == "[1, 2, 3]";
assert `${a:.3?}` == "[1, 2, 3, ...]";
```

A container renders its elements with the same spec it was given, so precision
and width reach every element. A tuple or struct never caps its own arity, but
its string and sequence fields honour the precision, and `${a:6?}` pads each
element rather than the whole list.

### The Formatter

Every format trait writes into a `Formatter`, which carries the parsed spec and
the string being built. Both types are in the prelude:

<!-- {"fixture":"spec_literals_formatter_decl.wado"} -->

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

test {
    let mut out = "";
    let mut f = Formatter {
        fill: ' ', align: Alignment::Right, sign_plus: false, alternate: false,
        zero_pad: false, width: -1, precision: -2, indent: 0, buf: &mut out,
    };
    Celsius { degrees: 21 }.fmt(&mut f);
    assert out == "21°C";
}
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

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
struct Celsius { degrees: i32 }

impl Display for Celsius {
    fn fmt(&self, f: &mut Formatter) {
        f.pad(`${self.degrees}°C`);
    }
}

test {
    assert `${Celsius { degrees: 21 }:>8}` == "    21°C";
}
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

### Display Output

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
- A range renders `start..<end` or `start..=end`, as written. A range has
  `Display` only when its bound type does.
- A closure or function value has no `Display`, as in Rust. `${f:?}` renders it.

### Inspect Output

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

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
let arr: List<i32> = [1, 2, 3];
assert `${arr:#?}` == "[\n  1,\n  2,\n  3,\n]";
assert `${Option::Some(42):#?}` == "Option::Some(\n  42,\n)";
assert `${Point { x: 1, y: 2 }:#?}` == "Point {\n  x: 1,\n  y: 2,\n}";
```

### Inspect Truncation

With no precision in the spec, `Inspect` of a string or a sequence caps it at
`Formatter::DEFAULT_SEQ_LIMIT`, 256 characters or elements, and marks the cut as
an explicit precision does, which keeps debug output readable. An explicit
precision replaces the cap. `Display` never applies it.

The cap covers `String`, `StrSlice`, `List`, `Array` and `Slice`. A sequence
that shows no element before the cut renders `[...]`, and the `#` form puts
`...` on a line of its own. A tuple, a struct, a `TreeMap` and a `TreeSet` are
never capped.

<!-- {"fixture":"spec_literals_format.wado"} -->

```wado
let long = "a".repeat(300);
assert `${long}`.len() == 300;
assert `${long:?}`.len() == 261;  // two quotes, 256 characters, and "..."
```

Rationale: [WEP: Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md),
[WEP: Format Traits](./wep-2026-02-01-format-traits.md),
[WEP: Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md).

## Tagged Template Literals

A path written directly before a template literal is a tag. The template then
denotes a call of that function on the template's holes, in their own types,
with the literal text around them, instead of a rendered `String`:

<!-- {"fixture":"spec_functions_tagged_template.wado"} -->

```wado
let q = sql`SELECT * FROM users WHERE id = ${id} AND name = ${user.name}`;
let s = String::raw`${dir}\bin\run.exe`;   // backslashes kept
assert q.query == "SELECT * FROM users WHERE id = ? AND name = ?";
assert s == "C:\\bin\\run.exe";
```

The tag is a function name or a static method path, with no whitespace before
the backtick. The backtick is a postfix on the path and binds as a call does.
Any other expression before a backtick, such as a call result or a
parenthesized expression, is a syntax error. A path naming a variant case or a
closure-typed binding is rejected as a tag. The literal is lexed exactly as an
untagged template, so every escape must still be one the lexer knows even where
the tag preserves it.

A tag is an ordinary function whose first parameter takes the template by value
and is bound by `ReflectTemplate`, the reflected kind of a template literal. The
template is the call's one written argument. Trailing parameters with defaults
are filled as in any call (see [Default Arguments](./spec-functions.md#default-arguments)). A first
parameter of any other type, `&T` included, is reported as a tag error. Nothing
on the declaration marks a function as a tag.

Each template shape has an anonymous type of its own, holding one field per
hole. The shape is the template's segments, specifiers, hole types and hole
source texts. So
`` tag`${a}` `` and `` tag`${b}` `` are two types, each instantiating the tag,
even where `a` and `b` share a type. The type is unnameable and reached only
through the bound; a diagnostic and `Reflect::type_name()` show it as its text
with each hole spelled by its type and specifier, `` `id = ${i32:04}` ``, cut
at 50 characters with `...`.

`ReflectTemplate` is
[sealed](./spec-reflection.md#sealed-traits), as the whole family is. Its
associated type `Holes` is the tuple of hole types,
and `Members` the tuple of hole handles `members()` returns. The tag walks the
holes with tuple `for-of`:

<!-- {"fixture":"spec_functions_tagged_template.wado"} -->

```wado
fn sql<T: ReflectTemplate<Holes = [..V]>, ..V: ToSqlParam>(t: T) -> SqlQuery {
    let mut query = "";
    let mut params: List<SqlParam> = [];
    for let h of ReflectTemplate::<T>::members() {
        query.push_str(h.lit());                // literal text before this hole
        query.push_str("?");
        params.push(h.get(&t).to_sql_param());  // the value, storage shared
    }
    query.push_str(ReflectTemplate::<T>::tail());
    return SqlQuery { query, params };
}

test "the tag sees each hole in its own type" {
    let q = sql`id = ${7} AND name = ${"ann"}`;
    assert q.params == [SqlParam::Int(7), SqlParam::Text("ann")];
}
```

A hole handle (`TemplateHole<T, V>`) answers `index()` (its position, from 0),
`lit()` / `raw()` (the preceding segment, escapes processed or preserved),
`get(&t)` (the value, `V`), `source()` (the expression text), `has_spec()`, and
`fmt(&t, f)` (rendering as the untagged template would).
`ReflectTemplate::<T>::tail()` and `raw_tail()` give the segment after the last
hole. Every answer but `get` and `fmt` is a constant. A hole handle is minted
only by `members()`.

`members()` walks a pack, so `Holes` is bound either as one (`[..V]`) or as the
empty tuple (`[]`, for a tag that reads only `tail()`). A concrete tuple
(`Holes = [List<i32>]`) is an error at the call. A bound on the pack
(`..V: ToSqlParam`) makes a hole whose type lacks it an error at the call,
naming that type.

A hole's type may not mention a type parameter of the enclosing item, since the
shape is minted once rather than per instantiation. A generic body passes its
tag a concrete value from its caller. The untagged template makes no shape, so
`` `${v}` `` over a `v: X` is accepted where `` format`${v}` `` is not.

Holes are evaluated once, left to right, before the tag runs. Each hole's value
is the one it had at its own position, so a later hole that writes to its
storage changes nothing the tag sees: `` format`${a} ${bump(&mut a)}` ``
renders what `` `${a} ${bump(&mut a)}` `` renders. A tag may carry effects,
which the caller declares as for any call, and return any type.

An untagged template means what the prelude's `format` tag means: each hole
rendered through its specifier into one buffer.

Rationale: [WEP: Tagged Template Literals](./wep-2026-01-10-tagged-template-literals.md).

## Tuple Literals

Bracket syntax `[...]` creates tuple values by default. This aligns with TypeScript conventions and JSON interoperability.

<!-- {"fixture":"spec_literals_tuples.wado"} -->

```wado
let pair = [1, "hello"];              // Type: [i32, String]
let triple = [42, "answer", true];    // Type: [i32, String, bool]
let single = [42];                    // Type: [i32] (1-tuple)
let empty_tuple: [] = [];             // Empty tuple (distinct from unit ())
let trailing = [1, 2, 3,];            // Trailing comma allowed
assert pair.1 == "hello" && triple.2 && single.0 == 42 && trailing.2 == 3;
```

The tuple type and its element access are in [Tuples](./spec-types.md#tuples).

### Value Spread

`..expr` inside a tuple literal splices the elements of the tuple `expr` into
it:

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
let a = [1, "hello"];
let b = [..a, true];   // b: [i32, String, bool]
let c = [42, ..a];     // c: [i32, i32, String]
assert b == [1, "hello", true];
assert c == [42, 1, "hello"];
```

The spread expression is evaluated exactly once:

<!-- {"fixture":"spec_functions_type_packs.wado"} -->

```wado
// make_pair() is called once, not twice
let t = [..make_pair(), 30];
assert PAIRS == 1;
assert t == [10, 20, 30];
```

A spread of a [type pack](./spec-functions.md#variadic-type-packs) value splices
however many elements the pack holds.

## List Literals

A bracket literal coerces to a `List` where the target type is known, as a
function parameter or a type annotation makes it. Elsewhere it is a tuple, and
`as List<T>` converts it.

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
fn takes_list(a: List<i32>) -> i32 { return a.len(); }

test {
    let t = [1, 2, 3];                      // Tuple [i32, i32, i32]: no context
    let numbers = [1, 2, 3] as List<i32>;   // explicit conversion

    assert takes_list([1, 2, 3]) == 3;      // implicit coercion to the parameter type
    let explicit: List<i32> = [1, 2, 3];    // implicit coercion to the annotation
    assert numbers == explicit && t.2 == 3;
}
```

What a `List` offers once built is in [Lists](./spec-types.md#lists).

Rationale: [WEP: Tuple and List Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md).

## Object Literals

A `{ … }` literal lists members separated by commas. A member is `key: value`,
a shorthand `name` that stands for `name: name`, or a `..base` spread. A key is
an identifier or a string literal, so `{ "x": 1 }` writes the key `{ x: 1 }`
writes. Any other key is an error.

<!-- {"fixture":"spec_literals_objects.wado"} -->

```wado
// Functional-update spread: seed from a base map, then override/add keys
let m2: TreeMap<String, i32> = { ..map, "x": 99, "w": 40 };
assert m2["x"] == 99 && m2["y"] == 20 && m2["w"] == 40;
```

The type expected of the literal decides what it builds: a named struct
([Struct Construction](./spec-types.md#struct-construction)), a collection such
as a map ([Collection Literal Coercion](#collection-literal-coercion)), or, with
no type expected, an [anonymous struct](./spec-types.md#anonymous-structs).

Rationale: [WEP: JSON Literal Compatibility](./wep-2026-01-18-json-literal-compatibility.md).

## Collection Literal Coercion

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

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
use { TreeMap } from "core:collections";

test {
    let arr: List<i32> = [1, 2, 3];
    let map: TreeMap<String, i32> = { width: 1920, height: 1080 };
    assert arr.len() == 3 && map["height"] == 1080;
}
```

Making a user type literal-constructible is one ordinary impl:

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
struct MyVec<T> { items: List<T> }

impl<T> From<Array<T>> for MyVec<T> {
    fn from(elements: Array<T>) -> MyVec<T> {
        return MyVec { items: List::<T>::from(elements) };
    }
}

test {
    let v: MyVec<i32> = [1, 2, 3];
    assert v.items.len() == 3;
}
```

### Impl Selection

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

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
type Scores = List<i32>;
let s: Scores = [90, 85];   // List::from, then `as Scores`
assert s[1] == 85;
```

### Implicit Conversion

A literal is implicitly converted to its target type through `From`. No other
expression is implicitly converted. A literal here is a number, string, char,
`bool`, `null` or byte literal, or a `[ … ]` or `{ … }` literal. A template
string, a variable and a call are not literals.

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
let v: List<Value> = [1, "x"];   // OK: every element is a literal
assert v[0] matches { Int(1) } && v[1] matches { String(s) && s == "x" };
```

A variable is not a literal, so it is not converted:

<!-- {"fixture":"spec_literals_implicit_conversion.wado"} -->

```wado
let v: List<Value> = [a, b];     // Error: write [Value::from(a), Value::from(b)]
let w: List<i64> = [x];          // Error: no implicit widening of `x: i32`
```

Literal typing runs first, so `42` against `i64` is an `i64`, not an
`i64::from`. `From` applies only where literal typing cannot reach the target:
against `Value`, the `1` in `{ n: 1 }` takes its default type `i32` and then
converts through `Value::from`. An element that reaches no conversion is
reported with the rule that refused it: the element is not a literal, or the
slot's type has no `From` for the element's type.

### `..base` Spread

A key-value literal may carry `..base` members, which merge through
`LiteralSpread`:

<!-- {"fixture":"spec_literals_spread_decl.wado"} -->

```wado
pub trait LiteralSpread with () {
    fn spread_literal(&mut self, base: Self);
}

test {
    let mut t = Tally { n: 1 };
    t.spread_literal(Tally { n: 2 });
    assert t.n == 3;
}
```

The members apply in source order and the last write wins. Each subexpression
is evaluated once, in source order. A spread may stand anywhere in the literal,
and there may be several. Each `base` must have the literal's target type.

<!-- {"fixture":"spec_literals_lists.wado"} -->

```wado
let base: TreeMap<String, i32> = { a: 1, b: 2 };
let more: TreeMap<String, i32> = { a: 9, d: 4 };
let m: TreeMap<String, i32> = { ..base, ..more, c: 3 };   // a: 9, b: 2, c: 3, d: 4
assert m["a"] == 9 && m["b"] == 2 && m["c"] == 3 && m["d"] == 4;
```

These are compile errors:

- a target type without a `LiteralSpread` impl;
- `{ ..base }` with no other member, which only copies `base`;
- the same key written twice as an explicit member.

A sequence literal cannot carry a spread: `[..xs, 4]` is a
[tuple spread](#value-spread). The struct forms of `..base`
are in [Struct Construction](./spec-types.md#struct-construction) and
[Anonymous Structs](./spec-types.md#composition).

Rationale: [WEP: Literal Coercion as `From<Array<…>>`](./wep-2026-08-24-literal-from-array.md),
[WEP: Literal Spread (`..base`)](./wep-2026-07-03-literal-spread.md).

## Compile-Time Literals

A literal that starts with `#` takes its value when the program is compiled:
where it stands in the source, or what a file holds.

| Literal                  | Type       | Value                             |
| ------------------------ | ---------- | --------------------------------- |
| `#file`                  | `String`   | Current source file path          |
| `#line`                  | `i32`      | Current line number (1-indexed)   |
| `#function`              | `String`   | Name of the enclosing function    |
| `#data`                  | `String`   | The module's `__DATA__` section   |
| `#include_str("path")`   | `String`   | External file content as a string |
| `#include_bytes("path")` | `ByteList` | External file content as bytes    |

<!-- {"fixture":"spec_literals_location.wado"} -->

```wado
fn example() -> String {
    return `Error at ${#file}:${#line} in ${#function}`;
}

test {
    assert example().ends_with("spec_literals_location.wado:4 in example");
}
```

`#function` names the function without type arguments or signature:

| Context                 | `#function` value            |
| ----------------------- | ---------------------------- |
| Free function           | `my_function`                |
| Method                  | `Point::distance`            |
| Method of `Box<String>` | `Box::name`                  |
| Closure                 | `parent_function::{closure}` |

### `#data`

`#data` is the raw text of the module's
[data section](./spec-lexical.md#data-section). In a file with no `__DATA__`
section it is a compile error.

In a file whose `__DATA__` section is `{"test": {}}`:

<!-- {"fixture":"spec_literals_location.wado"} -->

```wado
let config = #data;  // contains the __DATA__ section text
assert config.starts_with("{\"test\": {}}");
```

### `#include_str` and `#include_bytes`

`#include_str("path")` is the content of a file as a `String`. A file that is
not valid UTF-8 is a compile error. `#include_bytes("path")` is the raw bytes as
a `ByteList`, with no UTF-8 check.

The argument is a parenthesized string literal. Any other expression is a
compile error. A local path starts with `./` or `../` and resolves relative to the source file containing the expression, as [Module Path Validation](./spec-modules.md#module-path-validation) states for every path literal. A file that does not exist is a compile error.

<!-- {"fixture":"spec_literals_location.wado"} -->

```wado
let template = #include_str("./testdata/hello.txt");
let icon: ByteList = #include_bytes("./testdata/binary.dat");
assert template == "Hello, World!\n" && icon.len() == 5;
```

The file is read once, at compile time, and its content is a constant. Changing the file after compilation does not change the compiled program. The content is inserted as data and never expanded, so a file may include itself: `#include_str` of its own path yields its own source text.

Rationale: [WEP: Compile-Time File Inclusion](./wep-2026-03-02-include-str.md).

### Embedded Data

`List::<T>::from_le_bytes(bytes)` reads `bytes` as little-endian `T`s, back to
back, for any `T: FromLeBytes`: the fixed-width integers, `f16`, `bf16`, `f32`
and `f64`. It panics when the byte count is not a whole number of `T`s.

`builtin::array_new_data::<T>(bytes)` reads the bytes the same way into an
`Array<T>`, for a caller building its own container. It is evaluated at compile
time only. So its argument must be a byte string literal or `#include_bytes`,
`T` must be a numeric primitive, and the byte count must be a whole number of
`T`s. Each is a compile error otherwise.

<!-- {"fixture":"spec_control_flow_embedded_data.wado"} -->

```wado
let weights = List::<f32>::from_le_bytes(#include_bytes("./sub/spec_control_flow_weights.bin"));
let bias = List::<bf16>::from_le_bytes(b"\x80\x3f\x00\x40");
let want: List<bf16> = [1.0, 2.0];
assert weights == [1.0, 2.0] && bias == want;
```

### Call-site evaluation in default arguments

A [default argument](./spec-functions.md#default-arguments) resolves its names where it is declared, as [Where a Default Resolves](./spec-functions.md#where-a-default-resolves) states. `#file`, `#line` and `#function` are the exception: they evaluate at the call site, so a defaulted location parameter reports the caller.

<!-- {"fixture":"spec_literals_location.wado"} -->

```wado
pub fn log(msg: String, file: String = #file, line: i32 = #line) -> i32 { return line; }

test {
    assert log("started") == #line; // file/line report this call, not where `log` is defined
}
```

Where a default's own call fills a default in turn (`fn outer(x = loc())`), every one of these literals reports the outermost call (`outer(...)`).

`#data`, `#include_str` and `#include_bytes` in a default read the file that wrote the default. A struct field default is not redirected either: its location literals report the file that declares the struct.

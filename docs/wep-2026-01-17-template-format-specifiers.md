# WEP: Template Format Specifiers

## Context

A template string interpolates an expression with an optional format specifier:
`` `value: ${x:spec}` ``. The specifier language decides which formats are
expressible, which are silently wrong, and which are rejected.

Wado follows Rust's mini-language. Two of Rust's omissions are kept:

- No `g`/`G` (C's adaptive format). Its output switches between fixed-point and
  exponential by magnitude, so a caller cannot tell from the specifier which
  one a given value will take.
- No `p` (pointer). Wado has no pointer types.

| Feature             | Go         | Python     | Rust           | Wado    |
| ------------------- | ---------- | ---------- | -------------- | ------- |
| Fixed-point         | `%f`, `%F` | `:f`, `:F` | `{}` (default) | `${}`   |
| Exponential (lower) | `%e`       | `:e`       | `{:e}`         | `${:e}` |
| Exponential (upper) | `%E`       | `:E`       | `{:E}`         | `${:E}` |
| Adaptive            | `%g`, `%G` | `:g`, `:G` | none           | none    |
| Binary              | `%b`       | `:b`       | `{:b}`         | `${:b}` |
| Octal               | `%o`       | `:o`       | `{:o}`         | `${:o}` |
| Hex (lower)         | `%x`       | `:x`       | `{:x}`         | `${:x}` |
| Hex (upper)         | `%X`       | `:X`       | `{:X}`         | `${:X}` |
| Pointer             | `%p`       | n/a        | `{:p}`         | none    |
| Debug               | `%#v`      | `!r`       | `{:?}`         | `${:?}` |

## Decision

### Grammar

```text
interpolation := '${' expression [ ':' spec ] '}'
spec          := [[fill] align] ['+'] ['#'] ['0'] [width] ['.' precision] [type]
align         := '<' | '^' | '>'
type          := 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | 'f' | '?'
width         := digit+
precision     := digit+
```

Every part of `spec` is optional; `spec` itself is not. The grammar is closed —
each of these is a compile error:

| Input              | Error                                     |
| ------------------ | ----------------------------------------- |
| `${x:}`            | empty format specifier                    |
| `${x:d}`           | unknown format specifier `d`              |
| `${x:5x1}`         | unexpected `1` after the format specifier |
| `${x:.}`           | expected digits after `.`                 |
| `${x:99999999999}` | width too large (does not fit `i32`)      |

Rejecting the unknown is what keeps a printf-ism like `${x:08d}` from rendering
as `${x:08}` with the `d` dropped.

Whitespace around the specifier is trimmed, as it is around the expression:
`${ x : 5 }` reads as `${x:5}`.

`fill` is any character the interpolation scanner does not read as structure
while splitting `${…}`: not `'`, `"`, `` ` ``, `{`, `}`, nor a `/` that opens a
comment.

The parser separates `::` from a specifier `:` by lookahead:

| Written       | Read as                     |
| ------------- | --------------------------- |
| `${foo::bar}` | path in the expression      |
| `${foo::<T>}` | turbofish in the expression |
| `${foo:x}`    | specifier `x`               |

### Format types

| Type   | Trait      | Applies to          | Example                   |
| ------ | ---------- | ------------------- | ------------------------- |
| (none) | `Display`  | types with an impl  | `${42}` → `42`            |
| `?`    | `Inspect`  | every type          | `${42:?}` → `42`          |
| `f`    | `Display`  | types with an impl  | `${3.14159:.2f}` → `3.14` |
| `b`    | `Binary`   | integers            | `${42:b}` → `101010`      |
| `o`    | `Octal`    | integers            | `${42:o}` → `52`          |
| `x`    | `LowerHex` | integers            | `${42:x}` → `2a`          |
| `X`    | `UpperHex` | integers            | `${42:X}` → `2A`          |
| `e`    | `LowerExp` | integers and floats | `${42:e}` → `4.2e1`       |
| `E`    | `UpperExp` | integers and floats | `${42:E}` → `4.2E1`       |

A type that does not implement the trait a specifier names is a compile error.
`Display` is not derived for a struct or variant, so `${x}` on one without a
hand-written impl is rejected and `${x:?}` is the debug form. `Inspect` holds
for every type. See
[WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md).

`f` selects no trait of its own: `Display` already honours `precision`, so
`${x:.2f}` and `${x:.2}` render the same. It exists so a float format reads as
one.

`b`/`o`/`x`/`X` render a negative signed integer as the two's complement bit
pattern of its own width: `${-1 as i32:x}` → `ffffffff`.

`e`/`E` render an integer mantissa with trailing zeros stripped (`${1200:e}` →
`1.2e3`), or with `precision + 1` digits rounded half to even when a precision
is given (`${1250:.1e}` → `1.2e3`, `${1350:.1e}` → `1.4e3`). A carry moves the
exponent up a decade (`${99:.0e}` → `1e2`).

### Fill, alignment and width

`width` is a minimum, counted in characters — not bytes, not display columns:

```wado
`${42:5}`        // "   42"    default alignment
`${42:<5}`       // "42   "
`${42:^5}`       // " 42  "    left-biased when the padding is odd
`${42:>5}`       // "   42"
`${42:_>5}`      // "___42"
`${"あい":>6}`    // "    あい"  two characters, six bytes
```

The default alignment is right for every type. A fill character of any width
pads by whole characters.

### Sign

`+` renders a sign on a non-negative number; without it only a negative number
carries one. It has no effect on a non-numeric value.

```wado
`${42:+}`        // "+42"
`${-42:+}`       // "-42"
```

### Zero padding

`0` is a flag, not a width digit: `${x:0.2f}` is zero-padding plus a precision.
Padding goes after the sign and any radix prefix, and wins over an explicit
fill and alignment:

```wado
`${42:05}`       // "00042"
`${-42:08}`      // "-0000042"
`${-42:*<08}`    // "-0000042"
`${42:#08x}`     // "0x00002a"
`${-1200.0:012e}` // "-0000001.2e3"
```

### Alternate form

`#` sets `Formatter.alternate`; it selects no trait. Each implementation reads
the flag, or ignores it:

| Type     | Effect                 | Example    | Output     |
| -------- | ---------------------- | ---------- | ---------- |
| `x`, `X` | `0x` prefix            | `${42:#x}` | `0x2a`     |
| `b`      | `0b` prefix            | `${42:#b}` | `0b101010` |
| `o`      | `0o` prefix            | `${42:#o}` | `0o52`     |
| `?`      | Pretty-print, indented | `${p:#?}`  | multi-line |
| (none)   | Up to the `Display`    | `${42:#}`  | `42`       |
| `e`, `E` | None                   | `${42:#e}` | `4.2e1`    |

`${x:#X}` prefixes `0x`, not `0X` — the flag changes the prefix, the type
character changes the digits.

A hand-written `impl Display` may branch on `f.alternate`: `core:temporal`'s
`Instant` renders whole seconds plainly and milliseconds under `#`. Every
primitive ignores it.

### Precision

`precision` means something different per type, as it does in Rust:

| Operand                    | Meaning                      | Example                               |
| -------------------------- | ---------------------------- | ------------------------------------- |
| Float                      | decimal places               | `${3.14159:.2}` → `3.14`              |
| Integer under `e`/`E`      | mantissa decimal places      | `${12345:.2e}` → `1.23e4`             |
| Integer otherwise          | ignored                      | `${42:.2}` → `42`                     |
| `String`                   | maximum length in characters | `${"hello world":.5}` → `hello`       |
| `List` / `Array` / `Slice` | maximum number of elements   | `${[1, 2, 3, 4, 5]:.3}` → `[1, 2, 3]` |

Truncation is silent under `Display`. `Inspect` marks it: a `String` gets a
`...` after the closing quote, a sequence a `, ...` in place of the dropped
elements. The marker does not count toward the precision.

```wado
let s = "hello world";
`${s:.5}`        // hello
`${s:.5?}`       // "hello"...
`${s:.20}`       // hello world   (precision >= length: unchanged)

let a: List<i32> = [1, 2, 3, 4, 5];
`${a:.3}`        // [1, 2, 3]
`${a:.3?}`       // [1, 2, 3, ...]
```

One `Formatter` is shared with the elements a container renders, so `precision`
and `width` both reach them. A tuple or struct never caps its own arity, but
its `String` and sequence fields honour the active precision, and `${a:6?}`
pads each element rather than the collection.

### The default sequence cap

`Inspect` of a `String` or a sequence caps its length at `DEFAULT_SEQ_LIMIT`
(256) even with no precision in the spec, so debug output — power-assert
operand dumps included — stays readable. `Display` never applies the cap.

`Formatter.precision` therefore distinguishes three states, two of them
negative sentinels:

| Value                     | Meaning                                                                                           |
| ------------------------- | ------------------------------------------------------------------------------------------------- |
| `>= 0`                    | the precision the spec wrote                                                                      |
| `PRECISION_DEFAULT` (-2)  | none written; sequence `Inspect` uses `DEFAULT_SEQ_LIMIT`, every other operand treats it as unset |
| `PRECISION_INFINITE` (-1) | uncapped; sequence `Inspect` skips truncation                                                     |

`PRECISION_INFINITE` has no surface syntax — `.N` cannot write a negative — so
it is reachable only by building a `Formatter` directly.

### Interpolated expression

The expression before `:` is arbitrary, where Rust's is a name or an index:

```wado
`${x + 1}`
`${x * 2:x}`
`${p.x + p.y}`
`${arr.len()}`
```

## Consequences

Choosing between `${x}` and `${x:e}` is the caller's, on every float. That is
the cost of dropping `g`: the specifier names the notation, so the output shape
does not depend on the value.

A specifier that does not apply is a compile error rather than a rendering, so
adding a format trait to a type is a source-compatible change while removing
one is not.

### Known gaps

- [ ] Dynamic width and precision: the grammar takes literal digits, so neither
      can be computed. Closing it takes a nested interpolation
      (`${value:${width}.${precision}}`), which fits Wado's arbitrary-expression
      interpolations better than Rust's `width$` form — that one names
      argument-list positions Wado does not have. The interpolation scanner
      already tracks brace depth, so the spec text can carry a nested `${…}`;
      the parser and `format_spec` would have to keep it as an expression.
- [ ] A parameter with no meaning for its operand is dropped rather than
      rejected, against the closed grammar's intent: precision on an integer,
      `+` on a `String`, `#` on `e`/`E`. Only the type checker knows which
      combination is meaningless, so the check belongs in template synthesis.
- [ ] `0` pads a non-numeric value with zeros (`${true:08}` → `0000true`), and
      every type defaults to right alignment. Rust ignores the flag outside
      numbers and defaults a non-numeric value to left. `Formatter` carries no
      "this operand is a number" fact to decide with.

## References

- [WEP: Format Traits](./wep-2026-02-01-format-traits.md)
- [WEP: Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [WEP: Trait Derivation Policy](./wep-2026-06-25-trait-derivation.md)
- [WEP: Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Rust `std::fmt`](https://doc.rust-lang.org/std/fmt/)

# WEP: Byte Literals (`b'x'` and `b"..."`)

## Context

Wado has a byte-string literal `b"..."` (a constant `ByteList`, i.e.
`type ByteList = List<u8>`) and a char literal `'x'` (a `char`). Missing is the
single-byte analog `b'x'` — one `u8` written as a character, the companion to
`b"..."`. Today a byte-from-character is spelled `65` or `'A' as u8`, both losing
the intent.

This WEP adds `b'x'` and, in the same stroke, aligns the escape rules of both
byte forms: **no Unicode escapes**. `b"..."` currently accepts `\u{...}` when it
resolves to `≤ 0x7F` (e.g. `b"\u{41}"` → `[65]`); that is an inconsistency a byte
literal should not inherit, so both forms reject it.

## Decision

### `b'x'` — a byte literal

`b'x'` is an integer literal whose value is one byte and whose default type is
`u8`. It is the same category as `42`, differing only in spelling and default
type; everything else (coercion, range checks, arithmetic, patterns) is ordinary
integer-literal behavior.

```wado
let b = b'A';          // u8, 65
let n: i32 = b'A';     // 65   (numeric-literal coercion, like `let n: i32 = 65`)
let z = b'\xff';       // u8, 255
let bad: i8 = b'\xff'; // ERROR: 255 out of i8 range (same as `let bad: i8 = 255`)
match c { b'0'..=b'9' => .., b'a'..=b'f' => .. }   // range patterns over the byte
```

Default type is `u8`; it takes the ordinary numeric-literal coercion
([WEP 2026-01-12](./wep-2026-01-12-literal-type-conversion.md)), flowing without
a cast into any integer type whose range holds the value (always `0..=255`, so
`i8` only when `≤ 127`). This is more permissive than Rust's strict-`u8` `b'x'`,
matching Wado's "literals are flexible" rule; tightening later stays non-breaking.

### Shared byte content rule

Both `b'x'` and `b"..."` accept exactly these, each element one byte:

- an ASCII source char (`0x00..=0x7F`) → that byte; non-ASCII is an error;
- `\xNN` → any byte `0x00..=0xFF` (the only way to write `0x80..=0xFF`);
- a standard single-byte escape (`\n \t \r \\ \' \" \0 \b \f \/`), value `≤ 0x7F`.

`\u{...}` / `\uHHHH` are rejected in both — a Unicode escape denotes a scalar, not
a byte. Use `\xNN` for a raw byte, or a char literal `'…'` for a scalar. `b'x'`
additionally requires exactly one byte (`b''` and `b'ab'` are errors); `b"..."`
is unchanged apart from the `\u` rejection.

`b'` is unambiguous: Wado has no lifetimes, so `'` always opens a char/byte
literal.

## Consequences

- Byte parsers read intent-first (`b'{'`, `b'a'..=b'z'`), closing the
  `b'x':u8 :: b"...":ByteList` symmetry. `b'x'` reuses the existing integer
  literal, so it costs nothing beyond the front end.
- Rejecting `\u` in `b"..."` is a minor breaking change; the only affected code is
  a byte string using a `≤ 0x7F` Unicode escape, rewritable as `\xNN`. Worth it
  for one consistent byte-escape rule across both forms.

### Alternatives considered

- Strict `u8` (Rust): a special case against uniform literal flexibility, no gain.
- Desugar `b'x'` to a number literal: loses the source spelling for `format` /
  LSP hover and blocks a "byte literal" diagnostic.

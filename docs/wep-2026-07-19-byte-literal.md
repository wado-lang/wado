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
type; everything downstream (coercion, range checks, arithmetic, patterns,
codegen) is ordinary integer-literal behavior.

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

## Implementation

`b'x'` is threaded through the front end as a new `Literal` variant and **lowered
to `TirExprKind::IntLiteral` in reify**, rejoining the integer pipeline — so NIR,
WIR, and codegen are untouched (`u8` `IntLiteral` already lowers to `i32.const`).

- Lexer/token: add `TokenKind::ByteCharLit(String)` (raw text); dispatch
  `'b' if peek_second() == Some('\'') => lex_byte_char()` beside the `b"` arm
  (`lexer.rs:214`), `lex_byte_char()` modeled on `lex_char()` (`:1221`).
- AST/parser: add `Literal::Byte(String)` (`ast.rs:2614`); parse it in expression
  and pattern position (`parser.rs:3951`, `:2954`).
- Escape decoding (`elaborator/util.rs`): reject `\u` in `unescape_bytes` (`:159`)
  — this is the `b"..."` alignment — then add `unescape_byte` returning the single
  byte via `unescape_bytes` (error unless exactly one).
- Typing/reify: `resolve_literal` → `TypeTable::U8` (`expr.rs:345`); a
  `Literal::Byte` arm in `try_coerce_numeric_literal_inner` (`coercion.rs:48`);
  `reify_literal` → `IntLiteral { value: byte as u64, repr }` (`reify.rs:9332`).
- Patterns: `exh_literal` → `IntLit` (`expr.rs:2441`); pattern reify → `U128`
  (`reify.rs:9497`/`:9805`/`:10231`). Range patterns then work for free.
- Unparser + `format.fixtures/` entry preserve the `b'…'` spelling.
- TextMate: add a `string.quoted.single.byte.wado` rule beside the `b"` rule
  (`wado-cli/src/syntax.rs:298`); `mise run update-wado-vscode-grammar`.
  `syntax.rs` needs no change (`u8`/`char` already listed).
- Docs: `spec.md` (String/Character Literals) + `cheatsheet.md` note `b'x'` and
  the no-`\u` rule for both forms.

## Testing

Red/green TDD. Fixture `tests/fixtures/byte_literal.wado` mirroring
`byte_string_literal.wado`:

- [ ] default `u8`; each standard escape; `\xNN` incl. a high byte (`b'\xff'`);
- [ ] coercion (`let n: i32 = b'A'`), range failure (`let x: i8 = b'\xff'`),
      arithmetic (`b'A' + 1 == b'B'`), match + `matches` with range patterns;
- [ ] compile-error: `b''`, `b'ab'`, `b'あ'`, `b'\u{41}'`.

Byte-string alignment: add a compile-error fixture `b"\u{41}"` and confirm the
existing `byte_string_literal.wado` (no `\u`) still passes. Lexer unit tests near
`test_byte_string_literal` (`lexer.rs:1359`): `b'` recognized, `\x00` kept raw,
identifier ending in `b` (`crab`) not mis-lexed. `touch tests/e2e.rs` after
adding fixtures.

## Consequences

- Byte parsers read intent-first (`b'{'`, `b'a'..=b'z'`) with near-zero back-end
  cost (reuses `IntLiteral`), closing the `b'x':u8 :: b"...":ByteList` symmetry.
- Rejecting `\u` in `b"..."` is a minor breaking change; the only affected code is
  a byte string using a `≤ 0x7F` Unicode escape, rewritable as `\xNN`. Worth it
  for one consistent byte-escape rule across both forms.

### Alternatives considered

- Strict `u8` (Rust): a special case against uniform literal flexibility, no gain.
- Lower to `CharLiteral`: reaches the same codegen but carries `char` semantics
  needing re-typing to `u8`; `IntLiteral` already means "an integer value".
- Desugar `b'x'` to a number in the lexer: loses the source spelling for `format`
  / LSP hover and blocks a "byte literal" diagnostic.

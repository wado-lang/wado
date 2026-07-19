# WEP: Byte Literal (`b'x'`)

## Context

Wado already has two `b`-prefixed literal forms and a single-quoted char form:

- `b"..."` — a byte-string literal producing a constant `ByteList` (`type ByteList = List<u8>`). See the spec's [String Literals](./spec.md#string-literals) section.
- `'x'` — a char literal producing a `char` (a Unicode scalar).

What is missing is the single-byte analog of `b"..."`: a literal that denotes one `u8` value written as a character. Rust spells this `b'x'` (value `u8`), and it is the natural companion to `b"..."`. Today a `u8` from a character must be written either as a magic number (`65`) or via a cast (`'A' as u8`), both of which lose the intent "the byte for `A`".

Concrete motivating uses, all byte-oriented parsing/codegen where `u8` is the working type:

```wado
// Byte-level scanners (JSON, CBOR, HTTP, base64 all decode `List<u8>`)
if buf[i] == b'{' { depth += 1; }
match byte {
    b'0'..=b'9' => digit(byte),
    b'a'..=b'f' => hex(byte),
    b' ' | b'\t' | b'\n' | b'\r' => skip(),
    _ => error(),
}
let quote: u8 = b'"';
```

The goal of this WEP is a minimal, principled design that reuses the existing
literal pipeline rather than introducing a new value kind end to end.

## Decision

Add a **byte literal** `b'x'`: an integer literal whose value is a single byte
and whose default type is `u8`.

### Semantics

`b'x'` is exactly an integer literal — the same category as `42` — with two
differences: it is written as a character between single quotes with a leading
`b`, and its default type is `u8` instead of `i32`. Everything downstream
(coercion, range-checking, arithmetic, pattern matching, codegen) is the
ordinary integer-literal behavior. In particular:

```wado
let b = b'A';          // b: u8, value 65   (default type u8)
let n: i32 = b'A';     // 65                (integer-literal coercion, like `let n: i32 = 65`)
let z = b'\xff';       // z: u8, value 255
let bad: i8 = b'\xff'; // ERROR: 255 out of i8 range (same message as `let bad: i8 = 255`)
b'A' == b'A'           // true
match c { b'a'..=b'z' => ... }  // range pattern over the byte value
```

Framing it as "an integer literal that spells its value as a byte" is the whole
design. It is the simplest mental model and, as shown below, the cheapest
implementation: the literal is lowered to the existing
`TirExprKind::IntLiteral` node, so no new machinery is needed from TIR onward.

### Content and escapes

The content between the quotes must denote exactly **one byte** (`0x00..=0xFF`):

- A single ASCII source character (`0x00..=0x7F`) → that byte. A non-ASCII
  source character (e.g. `b'あ'`) is a compile error — it is more than one byte.
- A `\xNN` escape (two hex digits) → any byte `0x00..=0xFF`. This is the only
  way to write a high byte (`0x80..=0xFF`).
- A standard single-byte escape: `\n \t \r \\ \' \" \0` (and the rest of the
  shared single-char escape set). Each must resolve to a value `≤ 0x7F`, exactly
  as inside `b"..."`.
- `\u{...}` / `\uHHHH` are **rejected**: a Unicode escape denotes a scalar, not
  a byte, and may encode to multiple UTF-8 bytes. Use `\xNN` for a raw byte, or
  a char literal `'…'` when you want a scalar.
- `b''` (empty) and `b'ab'` (more than one byte) are compile errors.

This is the same byte-decoding rule already used for `b"..."`, restricted to
a length of exactly one. It reuses `elaborator/util.rs::unescape_bytes`.

### Type and coercion

Default type (no expected type from context) is `u8`. The literal participates
in the ordinary numeric-literal coercion ([WEP: Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md)): it flows without a cast
into any integer type whose range includes the value. Since the value is always
`0..=255`, it fits `u8`/`u16`/`u32`/`u64`/`u128`/`i16`/`i32`/`i64`/`i128` always,
and `i8` only when `≤ 127` — with the same range-check diagnostic an ordinary
integer literal of that value would produce.

This is deliberately more permissive than Rust (where `b'x'` is strictly `u8`).
The rationale is consistency: Wado's literals are uniformly flexible, `b'A'`
lowers to the identical IntLiteral node as `65`, and treating one flexibly and
the other strictly would be a special case with no payoff. It is also strictly
more permissive than a cast, so no existing code breaks.

### Grammar / disambiguation

`b'` is unambiguous. Wado has no lifetimes, so a single quote always opens a
char (or, with the `b` prefix, a byte) literal — there is no `'label` syntax to
collide with. The lexer already resolves `b"` before the identifier rule by
peeking the second character; `b'` is resolved the same way. A bare identifier
`b` followed by a char literal never arises in the expression grammar.

## Implementation Strategy

The literal is carried through the front end as its own AST `Literal` variant,
then **lowered to `TirExprKind::IntLiteral` in reify** so it rejoins the integer
pipeline. Concretely, only the front matter changes; NIR, WIR, and codegen are
untouched because a `u8` `IntLiteral` already lowers to `i32.const N`
(`lower/translate.rs:1107` → `wir_build/translate.rs:2078`).

### Lexer and token

- `token.rs` (~`:74`): add `TokenKind::ByteCharLit(String)` holding the raw
  source text between the quotes (escapes not interpreted at lex time — same
  convention as `CharLit`/`ByteStringLit`). Register it in the token-name macro
  list (`:269`) and the serializer (`:347`).
- `lexer.rs` (`:214`): add a dispatch arm **before** the identifier rule,
  mirroring the `b"` arm:

  ```rust
  'b' if self.peek_second() == Some('"')  => self.lex_byte_string(),
  'b' if self.peek_second() == Some('\'') => self.lex_byte_char(),   // new
  ```

  Add `lex_byte_char()` modeled on `lex_char()` (`:1221`): consume `b`, then the
  opening `'`, then reuse the same scan-to-closing-`'` logic, returning
  `TokenKind::ByteCharLit(raw)`. The existing char error kinds
  (`UnterminatedChar`, `EmptyCharLiteral`, `CharLiteralTooLong`, `:79`) are
  reused verbatim; length/byte-range validation is deferred to the elaborator
  (as with `b"..."`).

### AST and parser

- `ast.rs` (`:2614`): add `Literal::Byte(String)` (raw text), alongside
  `Char(String)` and `Bytes(String)`.
- `parser.rs` (`:3951`): add
  `TokenKind::ByteCharLit(raw) => Ok(self.consume_literal(Literal::Byte(raw), start_span))`
  in expression position, and the corresponding branch in pattern position
  (`:2954`, next to the `CharLit` case).

### Escape decoding

- `elaborator/util.rs` (`:159`): add
  `unescape_byte(raw: &str) -> Result<u8, String>`. Implement it via the
  existing `unescape_bytes` and assert the result is exactly one byte:

  ```rust
  pub(super) fn unescape_byte(raw: &str) -> Result<u8, String> {
      let bytes = unescape_bytes(raw)?;
      match bytes.as_slice() {
          [b] => Ok(*b),
          [] => Err("empty byte literal".to_string()),
          _  => Err("byte literal must be exactly one byte".to_string()),
      }
  }
  ```

  `unescape_bytes` already rejects non-ASCII source chars and `> 0x7f` non-`\x`
  escapes with the right messages, and decodes `\xNN` to a raw byte. `\u`
  escapes fall through `unescape_one` to a multi-byte/out-of-range error; if a
  sharper message is wanted, special-case `\u` here.

### Type-checking and reify

- `elaborator/expr.rs::resolve_literal` (`:345`): add
  `Literal::Byte(raw) => { validate via util::unescape_byte(raw); TypeTable::U8 }`.
- `elaborator/coercion.rs` (`:48`, `try_coerce_numeric_literal_inner`): add a
  `Literal::Byte(raw)` arm parallel to the `Literal::Number` arm — decode the
  byte, then range-check against the integer `target_type` with
  `util::check_int_range_positive` (the value is non-negative, so only the
  positive path is needed). This is what makes `let n: i32 = b'A'` type-check.
- `elaborator/reify.rs::reify_literal` (`:9332` region): add
  `Literal::Byte(raw) => TirExprKind::IntLiteral { value: unescape_byte(raw)? as u64, repr: raw.clone() }`,
  typed as `recorded_type` when present, else `TypeTable::U8`. This is the key
  step: from here on the literal _is_ an integer literal.

### Pattern matching

Byte literals appear in patterns (`b'{'`, `b'a'..=b'z'`). The pattern paths are
made to treat a byte literal as an unsigned integer literal:

- Exhaustiveness (`elaborator/expr.rs::exh_literal`, `:2441`): add
  `Literal::Byte(raw) => ExhPattern::IntLit(unescape_byte(raw)? as i128)`.
- Pattern reify (`elaborator/reify.rs`, `:9497` / `:9805` / `:10231`): add
  `Literal::Byte(raw) => TirLiteralPattern::U128(unescape_byte(raw)? as u128)`.

Range patterns (`b'a'..=b'z'`) fall out automatically: the range endpoints are
ordinary literal patterns, so once each endpoint reifies to a `U128` integer
pattern the existing integer-range machinery handles the rest.

### Formatter

`wado format` round-trips literals from the raw token text; the new
`Literal::Byte(raw)` preserves the source spelling (`b'x'`, `b'\n'`, `b'\xff'`),
so the unparser (`unparse.rs`, near the `Char`/`Bytes` arms) emits `b'` + raw +
`'`. Add a formatter fixture under `wado-compiler/tests/format.fixtures/`.

### Syntax highlighting

No change to `wado-compiler/src/syntax.rs` is required — it carries keyword/type
word lists, not per-literal regex, and `u8`/`char` are already listed. The
TextMate grammar is emitted by the `syntax` subcommand
(`wado-cli/src/syntax.rs`); add a `string.quoted.single.byte.wado` begin/end
rule for `b'`…`'` next to the existing `string.quoted.double.byte.wado` rule for
`b"` (`:298`), sharing the `#escapes` include so `\xNN` highlights. Regenerate
with `mise run update-wado-vscode-grammar`.

### What does NOT change

- NIR (`nir_value_graph.rs`), WIR (`wir.rs`, `wir_build/`), and codegen: a `u8`
  `IntLiteral` already lowers to `i32.const N`. Reusing `IntLiteral` means the
  byte literal inherits all of it — including constant folding, range patterns,
  and arithmetic — for free.
- `ByteList` / `List<u8>`: unrelated. A byte literal is a scalar `u8`, not a
  collection; only `b"..."` produces `ByteList`.

## Testing

Red/green TDD. E2E fixture `wado-compiler/tests/fixtures/byte_literal.wado`
mirroring the existing `byte_string_literal.wado`, covering:

- [ ] Default type `u8`: `let b = b'A'; assert b == 65;`
- [ ] Every standard escape: `b'\n' b'\t' b'\r' b'\\' b'\'' b'\"' b'\0'`.
- [ ] `\xNN` across the range, including a high byte: `b'\x00' b'\x7f' b'\xff'`.
- [ ] Coercion into other integer types: `let n: i32 = b'A'; assert n == 65;`.
- [ ] Range-check failure: `let x: i8 = b'\xff';` → `compile_error` (out of range).
- [ ] Arithmetic: `assert b'A' + 1 == b'B';`.
- [ ] Match with individual and range patterns, including exhaustiveness.
- [ ] `matches`: `assert byte matches { b'0'..=b'9' };`.

Compile-error fixtures for the rejected forms:

- [ ] `b''` (empty), `b'ab'` (multi-byte), `b'あ'` (non-ASCII), `b'\u{41}'`
      (unicode escape not allowed in a byte literal).

Lexer unit tests next to `test_byte_string_literal` (`lexer.rs:1359`):
`b'` recognized, `\x00` kept raw, and an identifier ending in `b` (e.g. `crab`)
not mis-lexed.

## Rollout Checklist

- [ ] Lexer + token (`ByteCharLit`), with unit tests.
- [ ] AST `Literal::Byte` + parser (expression and pattern position).
- [ ] `unescape_byte` in `elaborator/util.rs`.
- [ ] `resolve_literal`, `coercion`, `reify_literal` (→ `IntLiteral`).
- [ ] Pattern paths: `exh_literal` + pattern reify (→ `U128`).
- [ ] Unparser + formatter fixture.
- [ ] TextMate grammar rule + `mise run update-wado-vscode-grammar`.
- [ ] E2E + compile-error fixtures; `touch tests/e2e.rs` after adding files.
- [ ] Spec (`spec.md` Character/String Literals) + cheatsheet (`cheatsheet.md`
      Literals) note the `b'x'` form.

## Consequences

### Benefits

- Byte-oriented parsers read intent-first (`b'{'`, `b'a'..=b'z'`) instead of
  magic numbers or `'…' as u8` casts.
- Near-zero back-end cost: reusing `TirExprKind::IntLiteral` means NIR/WIR/codegen,
  constant folding, and range patterns need no new cases.
- Symmetry: `b'x'` : `u8` :: `b"..."` : `ByteList`, closing an obvious gap.

### Trade-offs

- More permissive than Rust (flexible integer coercion vs. strict `u8`). This is
  intentional and consistent with Wado's literal philosophy; it can be tightened
  later without breaking code, whereas loosening later could.
- One more `Literal` variant to thread through the front end. The pattern and
  reify sites are the same handful already touched for `Char`/`Number`, so the
  surface is small and well-bounded.

### Alternatives considered

- **Strict `u8` (Rust semantics)**: reject implicit coercion, require `as`.
  Rejected: it is a special case against the uniform "literals are flexible"
  rule for no benefit, since the value always fits the common integer targets.
- **Lower to `TirExprKind::CharLiteral`**: a byte and a char reach the same
  `i32.const` codegen, so this would also work. Rejected: `CharLiteral` carries
  `char` semantics (`TypeTable::CHAR`, char patterns), which would then need
  re-typing to `u8`; `IntLiteral` already means "an integer value of some
  integer type", which is exactly what a byte is.
- **No dedicated literal — desugar `b'x'` to a number in the lexer**: emit a
  `NumberLit("65")`. Rejected: it loses the source spelling, so `wado format`
  and LSP hover would show `65` instead of `b'A'`, and diagnostics could not
  say "byte literal".

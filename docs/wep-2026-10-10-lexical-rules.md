# WEP: Lexical Rules Left Open

## Context

The lexical chapters left several questions unanswered, and the compiler
answered each by accident:

- `_` lexed as an identifier, so `fn _() {}` and a field named `_` were
  accepted, and `let x = _;` failed only at name resolution.
- A surrogate pair of `\uHHHH` escapes wrote one character in a string, but a
  character literal counted it as two.
- A CRLF inside a string kept its CR, so a file's meaning depended on its line
  endings, and `#data` and doc comments carried the CR too.
- A backslash before a line break reported "invalid escape sequence", which
  does not say that Wado has no line continuation.
- `.5`, `5.`, a lone surrogate, `\x` outside a byte literal, and the byte-string
  escapes were rejected or accepted without a rule saying so.

## Decision

- `_` alone is the wildcard, not an identifier. The lexer gives it a token of
  its own, so every place that reads a name refuses it, and only the places
  that take the wildcard (a pattern, a parameter, a closure parameter, a type,
  `use _`, `with _`) accept it.
- A surrogate pair is one escape, in a character literal as in a string.
- Line breaks follow Rust: a line ends at LF or CRLF, a CRLF inside a literal, a
  comment or the data section reads as LF, and a CR that no LF follows is an
  error inside a literal.
- A backslash before a line break gets its own diagnostic, naming the missing
  line continuation as deliberate.
- `.5` and `5.` stay errors. Many languages accept `.5`, but it is rarely
  written, and a decimal point with a digit on each side keeps `5.max(3)` and
  `t.0.1` unambiguous.
- A lone surrogate stays an error, and `\x` stays a byte escape. A byte string
  takes a string's escapes (`\b`, `\f` and `\/` among them) and not a
  template's.

The rules are in [Lexical Structure](./spec-lexical.md) and
[Literals](./spec-literals.md).

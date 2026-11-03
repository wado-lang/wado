# Re Development Guide

Re is linear-time regular expressions in ECMA-262's syntax.
[WEP: Re](../docs/wep-2026-11-02-re.md) holds the design and why.

## Sources

The rules come from the specification, `vendor/ecma262/spec.html`: the chapter
`RegExp (Regular Expression) Objects`, read as the `u` flag reads it. Annex B
does not apply. A rule in the code cites the production or the section it
follows.

`vendor/test262` is the conformance corpus, read as data. No engine's source is
read, whatever its license. Node's `RegExp` may be run as a black-box oracle.

`\p{…}` takes what `core:icu` knows, at its Unicode version. Where that differs
from ECMA-262's tables or from an engine, `core:icu` is right.

## Layout

- `src/ast.wado` — the tree a pattern parses to.
- `src/parse.wado` — the parser, `rex`'s syntax included, and `PatternError`.
- `src/charset.wado` — code point sets, and what each named set holds.
- `src/utf8.wado` — a code point range as the UTF-8 byte sequences encoding it.
- `src/program.wado` — the tree compiled to Pike VM instructions over bytes.
- `src/pike.wado` — the Pike VM.
- `src/regex.wado` — `Regex`, its searches, `replace` and `split`.

## Running tests

```sh
wado test package-re
```

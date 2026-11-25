# Re Development Guide

Re is linear-time regular expressions in ECMA-262's syntax. The
`wado-package-standard` skill says where each kind of file goes.
[WEP: Re](../docs/wep-2026-11-02-re.md) holds the design and why.

## Architecture

A pattern parses to a tree, `rex`'s syntax included, and compiles to Pike VM
instructions over UTF-8 bytes. Three engines run a program: the Pike VM runs
every one, a lazy DFA finds a match's bounds for the programs it can run, and a
backtracker reads the groups of a short span the DFAs found. `Regex` picks the
engine for each search. A lookaround's body is a program of its own: an oracle
runs it over the whole text once, and a search reads the answer from a table. The `re` and `rex` tags build a pattern from a
template.

## Sources

The rules come from the specification, `vendor/ecma262/spec.html`: the chapter
`RegExp (Regular Expression) Objects`, read as the `u` flag reads it. Annex B
does not apply. A rule in the code cites the production or the section it
follows.

`vendor/test262` is the conformance corpus, read as data. No engine's source is
read, whatever its license. Node's `RegExp` may be run as a black-box oracle.

`\p{…}` takes what `core:icu` knows, at its Unicode version. Where that differs
from ECMA-262's tables or from an engine, `core:icu` is right.

## Tests

A test writes a pattern as a raw string, `#"\d+"#`, so that it reads as the
pattern it is. A cooked one needs a reason, such as a newline ending a `rex`
comment. A program uses `re` and `rex`, whose templates are raw already.

```sh
wado test package-re
```

`benchmark/README.md` says how to run the comparison with Node's `RegExp` and
the fuel test.

## Oracles

`tests/generated/` holds what the oracles answered, committed so that CI needs
neither:

- `mise run oracle-re` asks Node's `RegExp` about each case in
  `tests/oracle/cases.json` and `tests/generated/test262.json`.
- `mise run test262-re` reads test262's parse-phase negative tests, and runs
  test262's RegExp tests in Node to collect the patterns and inputs they use
  into `tests/generated/test262.json`. It then runs `oracle-re`.

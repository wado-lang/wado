# Jade Development Guide

Jade is JSON Schema Draft 2020-12 for Wado.

- [WEP: Jade](../docs/wep-2026-06-13-jade.md) — the design and why.
- [`research-format.md`](./research-format.md) — what the specifications and
  implementations say about the date and time formats.

## Principles

Compliance with the specification comes first. Whether to comply is never an
open question: the answer is always yes, because a schema means the same thing
to every tool only when each one follows the specification.

## Sources

The rules come from the specification: JSON Schema Core and Validation, Draft
2020-12. `vendor/JSON-Schema-Test-Suite/tests/draft2020-12/` is the conformance
corpus, read as data. No validator's source is read.

## Formats

A `format` means what Draft 2020-12 defines, read strictly: by the grammar the
specification cites, as the Test Suite's `optional/format/` cases hold it. Jade
defines no format of its own and writes no format name the specification does
not define, whatever another tool or registry uses. A derived schema takes a
`format` only when every text its type writes meets it.

`x-wado-format` names the Wado type a string is the text of, in symbol notation
(`core:temporal#Instant`). It is an annotation, which no validator asserts:
JSON Schema forbids an `x-` keyword from affecting evaluation.

## Layout

- `src/document.wado` — `Document`, the JSON tape every schema and instance is
  read into.
- `src/schema.wado` — `Schema`, one field per keyword, and its reader.
- `src/derive.wado` — `JsonSchema`, a schema derived from a Wado type.
- `src/validate.wado` — `Validator`, a schema compiled and held against
  documents, and `SchemaSet`, the documents its references reach.
- `meta/` — Draft 2020-12's meta-schemas as json-schema.org serves them, under
  the specification's license. A reference reaches them without their being
  supplied.
- `tools/suite.wado` — writes `tests/generated/` from the Test Suite
  (`mise run suite-jade`).

## Running tests

```sh
wado test package-jade
```

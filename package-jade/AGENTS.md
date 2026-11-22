# Jade Development Guide

Jade is JSON Schema Draft 2020-12 for Wado. The `wado-package-standard` skill
says where each kind of file goes.

- [`README.md`](./README.md) — the user manual.
- [`docs/design.md`](./docs/design.md) — the design.
- [`TODO.md`](./TODO.md) — open work.
- [`docs/research-format.md`](./docs/research-format.md) — what the
  specifications and implementations say about the date and time formats.

## Architecture

Every schema and instance is read into `Document`, a JSON tape. The four layers
of [the design](./docs/design.md#four-layers) sit on it: `Schema`, one field
per keyword; `JsonSchema`, a schema derived from a Wado type; `Validator`, a
schema compiled and held against documents, with `SchemaSet` the documents its
references reach; and `generate`, Wado declarations written from a schema,
which the Kiln generator wraps. `assets/meta/` holds Draft 2020-12's
meta-schemas as json-schema.org serves them, under the specification's license,
so a reference reaches them without their being supplied. `tests/roundtrip/` is
a consumer package: it imports schemas through the generator, and a program it
evaluates can call `schema_for` on the types `generate` writes.

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

## Running tests

```sh
wado test package-jade
```

`tests/generated/` is written from the Test Suite by `tools/suite.wado`
(`mise run suite-jade`).

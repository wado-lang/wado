# Jade TODO

Open work on Jade. [`README.md`](./README.md) holds the design, and
[`AGENTS.md`](./AGENTS.md) the rules for every change.

This file lists what is not yet done, at the level of behaviour. Find the code
by searching, not by line numbers. Closed work belongs in the commit history.

An entry under "Waiting on the language" is planned work and says when it is
done. Every other entry is a gap: it states what is missing and what it admits,
not how to close it.

## Waiting on the language

- [ ] **B's refined keywords, once refined types land.** A refined type's value
  clauses become its schema's assertion keywords, read through `ReflectRefined`
  and a `Keywords` trait Jade implements for the standard library's clause
  types: a range becomes `minimum` and `maximum`. A condition clause, or a
  clause type with no `Keywords` impl, carries nothing to read, so the refined
  type takes its base's schema, which accepts values the decoder rejects. Done
  when each standard library clause type has a `Keywords` impl and a test.
- [ ] **B's per-instantiation definition names, once `TypeInfo` lands.** A
  definition's name comes from `Reflect::decl_name()`, which names the
  declaration alone: `Pair<i32>` and `Pair<String>` get one name, and so do two
  `Point`s from different modules. Done when each gets a definition of its own.
- [ ] **D's refined types, once refined types land.** Done when every assertion
  keyword a standard library clause type carries is generated as that clause,
  and B gives it back.

## B: a schema that says less than the type

- [ ] **A hand-written `Serialize` the schema does not see.** A type whose
  `Serialize` or `Deserialize` is hand-written, and whose members are visible,
  takes the structural blanket. Its schema then describes its fields, not its
  wire form.
- [ ] **A text type's schema admits what the type cannot read.** A leaf whose
  type is text, such as `Instant` or `Url`, asserts only `string`, with a
  standard `format` only where one fits strictly. `x-wado-format` names the type
  but asserts nothing. A document whose string the type cannot read, such as
  `"yesterday"` for an `Instant`, matches the schema but does not decode. Bytes
  are the same: `contentEncoding: "base64url"` is an annotation, so `"!"`
  matches a `ByteList`'s schema.
- [ ] **A map whose key is not a string.** `TreeMap<i32, V>` is written as an
  object whose keys spell integers. JSON Schema states that through
  `propertyNames`, and the mapping table does not cover it.
- [ ] **Some comments have no place in the schema.** An `enum` lists its
  members' wire names under one `enum` keyword, and a `flags` lists its bits
  under one `items`, so neither has a place for a single member's comment. A
  newtype's schema is its base's, so the newtype's own comment reaches no
  definition either.

## C: validation

- [ ] **The host name formats are not asserted.** `hostname`, `idn-hostname`
  and `idn-email` pass every string. Each needs IDNA: a `hostname` label may be
  an A-label (`xn--…`), whose Punycode decodes to a label IDNA2008's rules
  govern, and the `idn-` formats hold their U-labels to the same rules (RFC
  5890, and RFC 6531 for `idn-email`). Wado offers no IDNA, for the homograph
  attacks it admits: a label that looks like another, such as one spelled with
  Cyrillic letters that look Latin.
- [ ] **Errors are a flat list.** C reports its errors as a list rather than as
  the specification's standard output units.

## D: generated types

- [ ] **An assertion keyword D cannot write yet.** D refuses a schema with an
  assertion keyword its table does not read, such as `maxLength`, `pattern` or a
  `minimum` that bounds no integer type exactly. This is a stopgap until refined
  types land, not a rule of Jade's. Until then, a third party's schema with such
  a keyword generates nothing.
- [ ] **An unbounded integer is an `i64`.** `{"type": "integer"}` admits any
  integer, and D writes `i64`. A document whose integer is beyond `i64` matches
  the schema but does not decode. `core:json` writes an `i64` beyond
  ±(2^53 − 1) as a string, such as `"9007199254740992"`, which the schema
  rejects.
- [ ] **A `number` or a `Value` holds a number only approximately.** `number`
  becomes `f64`, and a schema of `true` or `{}` becomes `core:value`'s `Value`,
  which holds a number as an `i64`, a `u64`, an `i128`, a `u128` or an `f64`. A
  decimal no f64 holds, such as `0.10000000000000000000001`, decodes to the
  nearest f64, and `1e400` does not decode. `Value` also has cases JSON has no
  value for, such as `Bytes` and `Undefined`.
- [ ] **A `format` D reads is only an annotation.** A `format` that D maps to a
  type, such as `uri`, asserts nothing unless the validator is asked to assert
  formats, which Draft 2020-12 leaves off by default. So `"hello"` matches
  `{"type": "string", "format": "uri"}`, and the `Uri` D writes for it does not
  decode.
- [ ] **An `Instant` is not a `date-time`.** D writes `Instant` for
  `format: "date-time"`, though `Instant` reads and writes Temporal's grammar,
  not RFC 3339's. It reads a leap second, `23:59:60`, as `23:59:59`; none has
  been inserted since 2016, and the CGPM resolved in 2022 to end them. It
  refuses a fraction of more than nine digits, which RFC 3339 admits. It writes
  an expanded year, such as `+010000`, which `date-time` rejects.
- [ ] **A generated struct drops a key no property names.** A derived
  `Deserialize` skips a key that names no field. Under
  `additionalProperties: false`, which D accepts, a document that breaks the
  keyword still decodes. Without the keyword, the schema admits any other key,
  and a document decoded and written again loses it.
- [ ] **An absent property is written back as null.** A property the schema does
  not list as `required`, and that has no `default`, becomes `Option<T> = null`.
  An absent one decodes to `None`, which is written as `null`, so
  `{"type": "string"}` rejects what the struct writes back.

## Serde and the wider ecosystem

- [ ] **One error per request.** `DeserializeError` reports the first failure.
  A framework that answers with every failure in the body, as FastAPI does, has
  nothing that collects them.
- [ ] **A number no f64 holds is not written.** `core:serde` writes a number
  only from a Wado number type, so writing a `Document` or a `Schema` that holds
  a number no f64 holds exactly, such as `1e400`, is an error. Reading and
  validating it are exact.
- [ ] **Other drafts have no reader.** A Draft 7 or OpenAPI 3.0 schema, with
  `nullable` and `definitions`, has no reader. A `$schema` naming any
  meta-schema but Draft 2020-12's is an error, as Jade cannot validate the
  schema against it.

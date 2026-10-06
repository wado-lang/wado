# Grog TODO

Open work on Grog. [`AGENTS.md`](./AGENTS.md) says how to work here, and
[WEP: Grog](../docs/wep-2026-09-22-grog.md) holds the design.

Each entry says what is missing and what it admits, not how to close it.
Closed work belongs in commit history.

## Not read from a schema

- [ ] **A `.proto` import is unresolved.** `import "other.proto";` names a
  second file, but Kiln hands a generator one primary file and its inputs.
  Which of them a Grog invocation reports, and how a consumer names the
  set, is undecided. A field whose type another file declares stops the
  build naming the field, which rules out `type.proto`, `api.proto` and the
  conformance messages.
- [ ] **A `service` produces nothing.** The grammar accepts one, since a real
  `.proto` carries them. A program that talks to such a service writes the
  calls by hand.
- [ ] **An extension is ignored**, so its fields arrive as unknown fields.
- [ ] **A per-field `[default = ...]` is not read**, so such a field decodes
  absent as `None` rather than as the default.
- [ ] **A string literal's escapes stay as written**, so `"a\x62"` reads as
  `a\x62`.
- [ ] **An `option features.*` inside a `oneof` is ignored**, so its members
  inherit from the message instead.
- [ ] **An option in the aggregate form `features = { … }` is ignored.**
- [ ] **`features.utf8_validation` is not read.** See the `string` entry below.

## Departures from protobuf on the wire

- [ ] **A `required` field is `Option<T>`**, and decoding one that is missing
  is not an error.
- [ ] **A `string` is UTF-8 whatever the schema says.** It is a Wado `String`.
  Proto2, and an edition that sets `features.utf8_validation = NONE`, allow
  a `string` field to hold other bytes. Grog refuses such a message as a
  decode error.
- [ ] **A varint whose tenth byte carries more than one bit is refused.** Such
  a varint overflows 64 bits. The C++, Java and upb parsers drop the extra
  bits and read it.
- [ ] **`Any` is not decoded.** It carries a type URL and bytes, so decoding
  one means looking up a message by name at run time, and every declaration
  here is resolved at build time.

## Schemas `protoc` refuses and Grog accepts

- [ ] **A label need not fit the syntax:** `required` in proto3, `optional` or
  `required` in an edition, and a proto2 field outside a `oneof` with no
  label are all accepted.
- [ ] **Two enum values may share a number without
  `option allow_alias = true`.**
- [ ] **A field may take a number or name its message's `reserved` statements
  list.**
- [ ] **A dotted relative name resolves past the scope `protoc` stops at.**
  `protoc` resolves `A.B` inside the scope where it finds `A`, and refuses
  it when `B` is missing there. Grog tries the next scope out, so it can
  resolve the name to a different declaration.

## Packaging

- [ ] **The runtime and the generator are one package at one version.** A
  consumer that pins the runtime at one version and invokes the generator
  at another gets code written against an API the runtime may not have.
  Resolving two versions of one package side by side is a package-manager
  question, and it is not answered.

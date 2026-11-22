# Grog Development Guide

Grog is Protocol Buffers for Wado: a Kiln generator that reads a `.proto`, and
the runtime the generated code encodes and decodes through. The
`wado-package-standard` skill says where each kind of file goes.

- [`README.md`](./README.md) — the user manual.
- [`docs/design.md`](./docs/design.md) — the design.
- [`TODO.md`](./TODO.md) — open work.
- [`tests/proto/README.md`](./tests/proto/README.md) — where the `.proto`
  corpus comes from and how it is refreshed.

## Architecture

The generator parses a `.proto` with a Gale-built parser
(`assets/grammar/Protobuf.g4`) into schema declarations, and emits them as Wado
types that implement the runtime's `Message`, `Enumeration` and `Scalar`
traits. The runtime holds `encode`, `decode` and `merge`. `tests/roundtrip/` is
a consumer package: it imports schemas through Kiln and round-trips them.

## Sources

The rules come from the specification, `vendor/protobuf-spec`:
`content/programming-guides/encoding.md` is the wire format, and
`content/reference/protobuf/` the grammar.

## Running tests

```sh
wado test package-grog
```

That walks `tests/roundtrip/` as well. Loam decodes ONNX through Grog, so a
change to the runtime or to what the generator emits also runs
`wado test package-loam`.

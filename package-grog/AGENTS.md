# Grog Development Guide

Grog is Protocol Buffers for Wado: a Kiln generator that reads a `.proto`, and
the runtime the generated code encodes and decodes through. Development stays
inside this directory:

- [`TODO.md`](./TODO.md) — open work.
- [WEP: Grog](../docs/wep-2026-09-22-grog.md) — the design and why.
- [`tests/proto/README.md`](./tests/proto/README.md) — where the `.proto`
  corpus comes from and how it is refreshed.

## Sources

The rules come from the specification, `vendor/protobuf-spec`:
`content/programming-guides/encoding.md` is the wire format, and
`content/reference/protobuf/` the grammar. An implementation may be run as an
oracle, but its code is not where a rule comes from.

## Layout

- `src/lib.wado` — the runtime: `encode`, `decode`, `merge`, and the `Message`,
  `Enumeration` and `Scalar` traits the generated code implements.
- `src/parse.wado` reads a `.proto` into `src/schema.wado`'s declarations,
  through the Gale-built `grammar/Protobuf.g4`.
- `src/emit.wado` writes those declarations as Wado; `src/generator.wado` is
  the Kiln entry around it.
- `tests/roundtrip/` is a consumer package: it imports schemas through Kiln
  and round-trips them.

## Running tests

```sh
wado test package-grog
```

That walks `tests/roundtrip/` as well. Loam decodes ONNX through Grog, so a
change to the runtime or to what the generator emits also runs
`wado test package-loam/src`.

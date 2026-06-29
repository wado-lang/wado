# cm-catalog

An enumeration of the Component Model ABI surface as Wado `export` functions.
Each export is an identity — it returns its argument unchanged — named after the
type it carries, so it exercises lowering (the parameter) and lifting (the
result) for exactly one shape.

[`cm-catalog.wit`](./cm-catalog.wit) is the artifact: a self-describing WIT
document listing every covered shape. It is meant to be published to a registry
and reused by other toolchains as a fixed lift/lower test corpus.

## Covered

[`cm-catalog.wit`](./cm-catalog.wit) is the authoritative, complete list. In
short: the whole value-type surface (primitives, containers, named types, and
their nested compositions), plus the async handle types `future` and `stream`.

For `future`/`stream` two flavours appear:

- **Consume/produce** — a bare `future<T>` reads the payload from its argument
  and writes it into a fresh future, so the payload itself round-trips (not just
  the handle).
- **Pass-through** — a handle returned unchanged, exercising lift/lower of the
  handle slot itself (`stream<u8>` and the aggregate-embedded handles).

## TODO

Full intended scope; checked items are implemented.

**Value types**

- [x] Primitives (`bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`, `string`)
- [x] Containers (`list`, `tuple`, `option`, all four `result` forms)
- [x] Named types (`record`, `variant`, `enum`, `flags`, newtype)
- [x] Nested compositions

**`future<T>` (consume/produce)**

- [x] Scalar payloads (`bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`)
- [x] `string`
- [x] `record`
- [x] `option<_>`
- [x] `result<_, _>`
- [x] `list<_>`
- [x] `tuple<…>`

**`stream<T>`**

- [x] `stream<u8>` (pass-through)
- [x] `stream<T>` consume/produce — scalar element payloads (`stream<u32>`)
- [x] `stream<T>` consume/produce — aggregate element payloads (`stream<string>`, `stream<point>`)
- `stream<char>` is intentionally out of scope (rejected by the Component Model)

**Embedded handles (pass-through)**

- [x] `option<future>`, `result<future, _>`, `list<future>`, `list<stream>`,
      `tuple<future, _>`, a record with a `future` field

**Test oracle**

- [x] Async value read-back — assert the payload survives the round-trip, not
      only the handle

**Handles**

- [ ] `own<resource>` / `borrow<resource>` identity

## Regenerating the WIT

```sh
wado wit package-cm-catalog/src/lib.wado > package-cm-catalog/cm-catalog.wit
```

`wado-compiler/tests/wit.rs` re-emits this and asserts it matches the committed
file, so the artifact cannot drift from the source.

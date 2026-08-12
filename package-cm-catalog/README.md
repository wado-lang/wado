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
- [x] `flags` inside `option` / `list` / `tuple` — the CM width (one byte at ≤8
      labels) only shows up where the ABI reads a stride or an offset

**`future<T>` (consume/produce)**

- [x] Scalar payloads (`bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`)
- [x] `string`
- [x] `record`
- [x] `option<_>`
- [x] `result<_, _>`, including the unit-Ok form — `future<result<_, string>>`
      has the shape of the WASI transmission future, and only a WASI error-code
      on the Err side makes it one
- [x] `list<_>`
- [x] `tuple<…>`
- [x] `variant` / `enum` / `flags` payloads

**`stream<T>`**

- [x] `stream<u8>` (pass-through)
- [x] `stream<T>` consume/produce — scalar element payloads (`stream<u32>`)
- [x] `stream<T>` consume/produce — aggregate element payloads (`stream<string>`, `stream<point>`)
- [x] `stream<T>` consume/produce — `variant` / `enum` / `flags` element payloads
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

`cm_catalog_matches_committed_wit` in `wado-compiler/tests/integration/wit.rs`
is the generator of record: it re-emits the interface from the source, asserts
it matches this file, and prints the emitted text on mismatch. Edit the source,
run the test, and take what it prints — the artifact cannot drift from the
source.

```sh
cd package-cm-catalog && wado wit --lib
```

emits the same interface, but wrapped in the library world instead of
`world command`, so it reads the exports back without producing this file.

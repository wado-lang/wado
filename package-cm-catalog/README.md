# cm-catalog

An enumeration of the Component Model ABI surface as Wado `export` functions.
Almost every export is an identity, named after the type it carries: it returns
its argument unchanged, so it exercises lowering (the parameter) and lifting
(the result) for exactly one shape. A few exports instead read several params
back, to reach a calling convention an identity cannot.

[`assets/cm-catalog.wit`](./assets/cm-catalog.wit) is the artifact: a
self-describing WIT document listing every covered shape. It is meant to be
published to a registry and reused by other toolchains as a fixed lift/lower
test corpus.

## Covered

[`assets/cm-catalog.wit`](./assets/cm-catalog.wit) is the authoritative,
complete list. In short: the whole value-type surface (primitives, containers,
named types, and their nested compositions), plus the async handle types
`future` and `stream`. [`TODO.md`](./TODO.md) lists what is not covered yet.

For `future`/`stream` two flavours appear:

- **Consume/produce** — a bare `future<T>` reads the payload from its argument
  and writes it into a fresh future, so the payload itself round-trips (not just
  the handle). The test asserts the payload survives, not only the handle.
- **Pass-through** — a handle returned unchanged, exercising lift/lower of the
  handle slot itself (`stream<u8>` and the aggregate-embedded handles).

### Value types

- Primitives (`bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`, `string`)
- Containers (`list`, `tuple`, `option`, all four `result` forms)
- Named types (`record`, `variant`, `enum`, `flags`, newtype)
- Nested compositions
- `flags` inside `option` / `list` / `tuple` — the CM width (one byte at ≤8
  labels) only shows up where the ABI reads a stride or an offset
- A `variant` with no payload-bearing case, bare and inside
  `option` / `list` / `tuple` — it lays out as its bare discriminant, and the
  same stride-or-offset reads are what show that width
- `map<k, v>`: the same bytes as `list<tuple<k, v>>` under its own type
  constructor. One row per key shape that lowers differently (`string`, a
  scalar), a repeated-key round trip for the last-wins rule, and a `map` in an
  async call's params buffer

### `future<T>` (consume/produce)

- Scalar payloads (`bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`)
- `string`
- `record`
- `option<_>`
- `result<_, _>`, including the unit-Ok form — `future<result<_, string>>` has
  the shape of the WASI transmission future, and only a WASI error-code on the
  Err side makes it one
- `list<_>`
- `tuple<…>`
- `variant` / `enum` / `flags` payloads

### `stream<T>`

- `stream<u8>` (pass-through)
- Consume/produce of scalar element payloads (`stream<u32>`)
- Consume/produce of aggregate element payloads (`stream<string>`,
  `stream<point>`)
- Consume/produce of `variant` / `enum` / `flags` element payloads

`stream<char>` is out of scope: the Component Model rejects it.

### Embedded handles (pass-through)

- `option<future>`, `result<future, _>`, `list<future>`, `list<stream>`,
  `tuple<future, _>`, a record with a `future` field

### Calling conventions

- Narrow scalars in the indirect params buffer. Past four flat params an async
  call puts every param in one buffer, each at its tuple-layout offset, so a
  scalar stored wider than its CM size writes past its slot

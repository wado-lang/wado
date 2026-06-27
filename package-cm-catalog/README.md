# cm-catalog

An enumeration of the Component Model value-type ABI surface as Wado `export`
functions. Every export is an `identity` — it returns its argument unchanged —
named after the type it carries, so the export exercises both lowering (the
parameter) and lifting (the result) for exactly one type.

The generated [`cm-catalog.wit`](./cm-catalog.wit) is the artifact: a
self-describing WIT document listing every covered shape. It is meant to be
published to a registry and reused by other toolchains as a fixed lift/lower
test corpus.

## Scope

Covered (value types):

- Primitives: `bool`, `u8`–`u64`, `s8`–`s64`, `f32`, `f64`, `char`, `string`.
- Containers: `list`, `tuple`, `option`, and all four `result` forms
  (`result<o, e>`, `result<o>`, `result<_, e>`, `result`).
- Named types: `record`, `variant`, `enum`, `flags`, type alias (newtype).
- Nested compositions: `list<option<_>>`, `option<list<_>>`, `list<record>`,
  `result<list<_>, _>`, and so on.

Covered (async handle types):

- Bare `future<T>` identities that **consume** the argument (read the payload)
  and **produce** the result (write the payload into a fresh future), so the
  payload's lift/lower is genuinely exercised — not just the handle move.
- `stream<u8>` and handles embedded in each aggregate kind
  (`option<future>`, `result<future, _>`, `list<future>`, `list<stream>`,
  `tuple<future, _>`, a record with a `future` field) as identity
  pass-throughs, exercising lift/lower of a handle at an aggregate offset.

Deferred:

- Handles (`own`/`borrow` of resources) — a handle is not duplicable, so
  identity needs different boundary semantics.

### Async coverage

A bare `future<T>`/`stream<T>` identity moves a single i32 handle, so a
pass-through tests nothing about `T`. The faithful test is consume/produce:
read the payload from the argument, write it into a fresh handle. That exercises
`future.read`/`future.write` for `T`, which the compiler currently lowers only
for **scalar** payloads (integer / float / `bool` / `char`). So bare `future<T>`
covers those; aggregate payloads (`string`, `record`, `option`, `result`,
`list`, `tuple`) and the streaming consume/produce path are pending compiler
support and appear here only as embedded pass-throughs. `stream<char>` is also
absent — the Component Model rejects it for now.

## Regenerating the WIT

```sh
wado wit package-cm-catalog/src/lib.wado > package-cm-catalog/cm-catalog.wit
```

`wado-compiler/tests/wit.rs` re-emits this from the source and asserts it
matches the committed `cm-catalog.wit`, so the artifact cannot drift from the
emitter.

## Note on compilation

`wado wit` enumerates every `export fn`, so the WIT artifact is complete today.

`wado compile --lib package-cm-catalog` synthesizes and emits the lift/lower
adapters for each export under a library world named after the package. The
whole value-type surface above compiles and round-trips: `lift(lower(x)) == x`
holds for every export, verified by `wado-compiler/tests/cm_catalog.rs` at both
`-O0` and `-O2`. See WEP `wep-2026-05-02-wit-interoperability.md`
("World-less libraries").

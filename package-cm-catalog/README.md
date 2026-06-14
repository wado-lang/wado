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
- Named types: `record` (including empty), `variant`, `enum`, `flags`, type
  alias (newtype).
- Nested compositions: `list<option<_>>`, `option<list<_>>`, `list<record>`,
  `result<list<_>, _>`, and so on.

Deferred:

- Async types (`future`, `stream`) — identity is not meaningful for a
  single-use value; they get their own catalog.
- Handles (`own`/`borrow` of resources) — a handle is not duplicable, so
  identity needs different boundary semantics.

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
adapters for each export under a library world named after the package. It
currently compiles the primitive value types; containers, the four `result`
forms, `string` returns, and user-defined named types (record/enum/variant/
flags/newtype) are still being wired (the per-type status is the point of this
catalog — it surfaces exactly which CM shapes the lift/lower path supports). See
WEP `wep-2026-05-02-wit-interoperability.md` ("World-less libraries").

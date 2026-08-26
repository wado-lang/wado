# WEP: The Component Model `map<K, V>` Type

## Context

The WASI Subgroup voted on 2026-08-06 to adopt the Component Model `map<K, V>`
type (🗺️), and [WASI 0.3.1](https://github.com/WebAssembly/WASI/blob/main/specifications/wasi-0.3.1/Overview.md)
(released 2026-08-11) names it a required feature of the baseline. `map` is no
longer a proposal Wado can wait out: an interface in the 0.3.1 line may use it,
and a component claiming 0.3.1 must lift and lower it.

`map<K, V>` is a *specialized* value type. The Component Model despecializes it
to `list<tuple<K, V>>` (`Explainer.md`, "Specialized value types"), so it shares
that type's canonical ABI exactly — only the type-constructor byte differs
(`0x63`). What the specialization buys is intent: a bindings generator is told
to present an associative container rather than a list of pairs. The spec adds
no key-uniqueness or ordering guarantee, and states that a generator *may*
deduplicate and reorder as long as the last pair for a key wins.

Wado has the container this is asking for. `TreeMap<K, V>` (`core:collections`)
already iterates in insertion order and already answers `{ k: v, … }` literals
through `From<Array<[String, V]>>`. What it does not do is cross the component
boundary: `is_param_type_supported` / `is_return_type_supported`
(`wado-compiler/src/component_model.rs`) admit only `List`, `Option`, `Result`,
`Tuple`, `Stream`, `Future`, so `export fn f(m: TreeMap<String, u32>)` is
rejected. On the way in, `wado-from-idl` drops `TypeDefKind::Map` into its
`Named("Unknown")` fallback.

The toolchain is ready. `wit-parser`, `wasmparser`, and `wasm-encoder` at the
pinned 0.252 generation all carry `map`, and wasmtime 47 implements
`InterfaceType::Map`. Nothing here waits on the wasmtime upgrade.

## Decision

### `TreeMap<K, V>` is the Wado spelling of `map<K, V>`

| Direction         | Mapping                        |
| ----------------- | ------------------------------ |
| Wado → WIT / CM   | `TreeMap<K, V>` → `map<k, v>`  |
| WIT / CM → Wado   | `map<k, v>` → `TreeMap<K, V>`  |

No new Wado type. `TreeMap` is what a Wado author already reaches for, and
insertion-order iteration is exactly the "last write wins, order unspecified"
contract `map` states.

### `K` is restricted to the Component Model key types

The spec's `keytype` is a deliberate subset of `valtype`:

```
keytype ::= bool | s8 | u8 | s16 | u16 | s32 | u32 | s64 | u64 | char | string
```

At a component boundary, `K` must be one of `bool`, `i8`–`i64`, `u8`–`u64`,
`char`, `String`. A `TreeMap` with any other key is diagnosed at the boundary,
naming the key type and the admissible set — not silently degraded to
`list<tuple<K, V>>`, which would make the emitted WIT disagree with the source.
`TreeMap` itself is unchanged: any `K: Ord` remains legal away from the
boundary.

`f32` / `f64` are absent from `keytype` and stay absent here.

### The ABI is the `list<tuple<K, V>>` ABI

Since `map` despecializes to `list<tuple<K, V>>`, its canonical ABI is that
type's, unchanged:

- flat: `[i32 ptr, i32 count]`
- in memory: 8 bytes, align 4
- elements at `ptr + i * cm_size(tuple<K, V>)`

`cm_size` / `cm_align` / `cm_flatten` answer for `TreeMap<K, V>` exactly what
they answer for `List<[K, V]>`. Only the component *type section* differs, where
`CmDefined::Map(k, v)` encodes `0x63` instead of a list of a tuple.

### Lift and lower delegate through `List<[K, V]>`

The CM binding synthesizer does not learn `TreeMap`'s representation. `TreeMap`
is an AA-tree over two backing lists with a tombstone flag; a lift that built
that layout directly would couple `cm_binding` to a private invariant of
`core:collections` and break the moment the tree is retuned.

Instead the binding reuses the list path it already has and converts at the
ends, through two methods on `TreeMap`:

```wado
pub fn from_entries(entries: List<[K, V]>) -> TreeMap<K, V>
pub fn to_entries(&self) -> List<[K, V]>
```

- **Lift** (`map` → `TreeMap`): `synthesize_lift_list` with element type
  `[K, V]`, then `TreeMap::<K, V>::from_entries(…)`.
- **Lower** (`TreeMap` → `map`): `m.to_entries()`, then the existing list lower.
- **Free**: the list free, unchanged — the buffer is a list buffer.

This costs one intermediate `List<[K, V]>` per crossing. That is the price of
not encoding a private layout into the ABI, and it is a boundary cost, not a
hot-path one. A later pass can fuse the intermediate away without changing this
design.

`from_entries` inserts in list order, so a duplicate key takes the last pair's
value — the rule the spec states. `to_entries` yields insertion order and skips
tombstones, so it never emits a duplicate. Round-tripping a `TreeMap` is
therefore the identity, and round-tripping a duplicate-bearing `map` normalizes
it, both of which the spec permits.

### Feature gating

`map` is ungated inside Wado: a component Wado emits targets the WASI 0.3.1
baseline, where 🗺️ is required. Wado's own validation already admits it
(`WasmFeatures::all()`). The one switch to flip is the embedder's —
`Config::wasm_component_model_map(true)` in `wado-cli`'s runtime — so `wado
run` / `test` / `serve` accept what the compiler emits.

## Consequences

- `wado wit` emits `map<k, v>` for a `TreeMap<K, V>` in an exported signature,
  and `wado-from-idl` / `wit_consume` produce `TreeMap<K, V>` for `map<k, v>`,
  so the `wasi:*` stdlib regenerates cleanly when a 0.3.1 interface adopts one.
- `package-cm-catalog` gains `map` rows. The catalog already carries
  `id_assoc_array(List<[String, u32]>)` — the despecialized shape — so the new
  rows pin the *specialization*: same bytes, different type constructor.
- A `TreeMap` key outside `keytype` is a boundary diagnostic, not a silent
  fallback.

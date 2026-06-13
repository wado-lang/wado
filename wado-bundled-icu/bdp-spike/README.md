# bdp-spike — BlobDataProvider separation (technical-validation spike)

A proof-of-concept that splits ICU4X into a **data-free feature component** plus
a **shared data component**, instead of baking CLDR/Unicode data into every
component via `compiled_data` (the model used by `../` = `wado-bundled-icu`).

This validates the "shared part as its own component, others use it" idea: the
Unicode **data** lives once in a blob that a feature component loads at runtime
through ICU4X's [`BlobDataProvider`], either fed by the host or supplied by a
sibling **data** component composed in via the Component Model.

[`BlobDataProvider`]: https://docs.rs/icu_provider_blob/

## Pieces

| dir | crate | role |
|---|---|---|
| `datagen/` | native tool | slices the casemap data markers into a postcard **blob** (`icu_provider_export` + `icu_provider_source`) |
| `casemap/` | wasm component | **data-free** feature: `icu_casemap` with `default-features = false` (no `compiled_data`); imports `data`, exports `casemap` |
| `data/` | wasm component | **shared data**: bakes the blob, exports `data` |
| `runtime-check/` | native | instantiates under wasmtime and asserts correct Unicode output |

WIT (`casemap/wit/world.wit`): the feature component
`import`s `wado:icu-bdp/data` (`get-casemap-blob: func() -> list<u8>`) and
`export`s `wado:icu-bdp/casemap` (`fold`, `uppercase`). It builds a
`CaseMapper` lazily on first call via `CaseMapper::try_new_with_buffer_provider`
over a `BlobDataProvider` wrapping the imported blob.

## Reproduce

```sh
./build.sh
```

The script runs datagen → builds both components → composes them → runs the
checks. Output artifacts (`*.wasm`, `casemap.blob`) are git-ignored.

### Generating the blob

`datagen` uses `SourceDataProvider::new()`, which downloads the CLDR/Unicode
data icu4x 2.2 was tested against. Its HTTP client (`ureq`, bundled webpki
roots) rejects a corporate/sandbox TLS-intercepting proxy with
`InvalidCertificate(UnknownIssuer)`. The download step is skipped when the file
already exists in the source cache, so pre-seed it with a client that trusts the
system store (e.g. `curl`):

```sh
CACHE=${ICU4X_SOURCE_CACHE:-/tmp/icu4x-source-cache}/github.com
mkdir -p "$CACHE/unicode-org/icu/releases/download/release-78.1rc" \
         "$CACHE/unicode-org/cldr-json/releases/download/48.2.0"
curl -sSL -o "$CACHE/unicode-org/icu/releases/download/release-78.1rc/icu4x-icuexportdata-78.1rc.zip" \
  https://github.com/unicode-org/icu/releases/download/release-78.1rc/icu4x-icuexportdata-78.1rc.zip
curl -sSL -o "$CACHE/unicode-org/cldr-json/releases/download/48.2.0/cldr-48.2.0-json-full.zip" \
  https://github.com/unicode-org/cldr-json/releases/download/48.2.0/cldr-48.2.0-json-full.zip
```

(The tags come from `SourceDataProvider::TESTED_CLDR_TAG` /
`TESTED_ICUEXPORT_TAG`.)

## Result — it works

`runtime-check` passes in **both** scenarios, proving the feature component is
genuinely data-free and the blob can arrive either way:

1. **feature + host-supplied data import** — host fulfils `get-casemap-blob`.
2. **composed component, empty host linker** — `data` is satisfied internally by
   the shared `data` component (`wasm-tools compose`); the composite imports
   nothing.

Both produce correct locale-aware Unicode (Turkish dotted-`İ`, Greek full
uppercase, `ß`→`ss` fold) — i.e. the CLDR data is live across the boundary even
though the feature component bakes none of it.

## Sizes (casemap only, `-Os`, LTO, stripped)

| artifact | size | contents |
|---|---:|---|
| `casemap.blob` | 23.4 KB | the sliced casemap data (`CaseMapV1` + `CaseMapUnfoldV1`) |
| `casemap-feature.wasm` | 45 KB | algorithm + `BlobDataProvider`/postcard + langid parse + glue; **no data** |
| `data-provider.wasm` | 32 KB | blob (23.4 KB) + component glue |
| `composed.wasm` | 78 KB | feature + data, import-free |

For comparison, baking casemap data directly (`../`, the `compiled_data`
model) lands the locale+casemap slice at ~92 KB.

## What this proves, and what it's worth

- **Feasibility ✅.** A feature component can carry zero Unicode data and load it
  at runtime from a postcard blob, in `no_std`/`wasm32-unknown-unknown`, and the
  blob can be delivered by another component over a CM `import` (the shared-data
  architecture), not just by the host.
- **Slicing works.** datagen emitted a 23 KB casemap-only blob; the blob is the
  size knob, independent of the feature code.
- **Single-feature size is ~neutral.** For casemap alone, BDP is not a size win:
  the `BlobDataProvider` + postcard deserialization code in the feature roughly
  offsets what `compiled_data` would bake. The payoff is structural, below.

### Where the real win is (next steps)

1. **Cross-feature data dedup.** ICU components share data markers — e.g. the
   collator uses the **normalizer**'s data internally. With `compiled_data`,
   collator and normalizer each bake their own copy. With BDP, both feature
   components load from **one shared blob** where datagen stores each marker
   once: `Collator::try_new_with_buffer_provider` pulls *both* collation and
   normalization markers from the same provider. So the data is shared at the
   data level (one blob, deduped markers) even though each feature keeps its own
   algorithm code — which is exactly the dedup that exporting *interfaces*
   cannot achieve. Proving this with a collator+normalizer pair is the logical
   follow-up.
2. **Per-deployment slicing.** datagen bakes only the markers an app's features
   actually use, so the shared blob never carries unused data.
3. **Data/code separation.** Feature components become tiny, reusable, and
   independent of the data version; the blob is portable and cacheable.

### Caveat

`BlobDataProvider` deserializes (zero-copy via `Yoke`) from the blob held in the
feature component's own linear memory, so the data is materialized per feature
instance at runtime rather than borrowed from a single static image. That is the
cost of the CM boundary: components don't share memory, so a shared *data*
component dedups the **stored** blob, not the per-instance working set.

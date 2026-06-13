# WEP: Compile-Time Data Providers (bundled ICU as the first consumer)

## Context

Some standard-library capabilities are dominated not by code but by large,
statically-bakeable data: Unicode/CLDR tables (ICU), time-zone databases,
i18n message catalogs, font glyph sets. Baking all of it into every program is
untenable — see the measured ICU spike under `wado-bundled-icu/`, where the full
library is ~3.7 MB and a single feature (segmentation) is ~4 MB of data alone.

A companion findings document, [research: splitting large libraries](./research-library-splitting.md), establishes the levers and the
hard constraints (separate component memories; only bytes cross the boundary;
dedup follows genuine runtime data dependencies, not taxonomy). The
`wado-bundled-icu/bdp-spike/` proof-of-concept then demonstrated the concrete
mechanism end-to-end: a feature component built without baked data, loading a
sliced postcard blob at runtime through ICU4X's `BlobDataProvider`, with the
blob supplied either by the host or by a separate data component composed in.

What remains is the toolchain integration: how a Wado program declares what it
needs, and how the compiler produces a minimal, per-program data slice. This WEP
proposes a general mechanism for that and adopts ICU as its first consumer.

### Why not Kiln

[Kiln](./wep-2026-04-12-kiln.md) already provides the right plumbing —
sandboxed, deterministic, content-addressed compile-time generators invoked from
`use ... with` sites via `CompilerHost::run_generator`. But Kiln is deliberately
narrowed to "IDL → Wado source": its generators are Wado packages, its input is
a user schema file, and its output is `.wado` source. Data provisioning needs
three things Kiln intentionally does not do:

- Emit a binary data asset (and wire it to a prebuilt component), not source.
- Be driven by which exported symbols are actually used, not by a schema file.
- Run a provider that may be authored in any language (ICU's slicer is ICU4X /
  Rust), since the host only ever executes an opaque wasm component.

So this is a sibling mechanism that reuses Kiln's infrastructure (sandbox,
cache, host-delegated execution) under a different contract, rather than a
widening of Kiln's charter.

## Decision

### The mechanism: compile-time data providers

A data provider is a wasm component implementing a `data-provider` world. The
compiler invokes it during compilation, sandboxed and cached exactly like a
Kiln generator, but with a contract specialized for data slicing. The source
language of the provider is unconstrained; the contract is defined over the
wasm component, not over Wado source.

Surface and data are separated:

- The type surface of a provider-backed module (e.g. `core:collation`) is a
  prebuilt, bundled WIT interface. Type-checking resolves names against it with
  no provider involvement, so it works offline and is fast.
- The provider produces only data. It is invoked once the elaborator has
  resolved reachability (see below), and returns the bytes to embed.

Provider contract (sketch; final WIT regenerated via
[Wado→WIT mapping](./wep-2026-01-29-wit-wado-mapping.md)):

```wit
package core:provider;

interface types {
  record request {
    module: string,          // "core:collation"
    symbols: list<string>,   // union of imported names across all use-sites
    options: list<u8>,       // canonical-encoded union of mergeable `with { ... }`
    datasets: list<string>,  // names of bundled archive entries this provider may read
  }
  record diagnostic { /* span, severity, message */ }
  record response {
    data: list<u8>,          // the sliced per-program asset
    diagnostics: list<diagnostic>,
  }
}

world data-provider {
  // Reuses Kiln's host. `read-file` is generalized to a name-keyed `read-asset`
  // so a provider pulls bundled archive entries exactly the way a Kiln generator
  // reads user files — no separate host interface is added.
  import core:kiln/kiln-host;
  use types.{request, response};
  export provide: func(req: request) -> response;
}
```

The op→marker mapping lives entirely in the provider — it is ICU-version-specific
(e.g. that `collator.compare` also pulls `NormalizerNfd*`). The compiler passes
only Wado symbol names and stays ICU-agnostic. The provider's correctness is
guarded by a test that records the markers each op's constructor actually
requests and asserts the map covers them, catching drift on ICU upgrades.

### Compiler aggregation phase

Unlike Kiln (one content-addressed invocation per `use ... with` site), a data
provider is invoked once per (feature, program):

1. Resolve `core:collation` etc. as provider-backed; type-check against the
   bundled surface.
2. Collect every `use` site of that module across the program and take the union
   of imported symbol names and of `with { ... }` options. Each option must be
   _mergeable_: list options (e.g. `locales`) union; scalar options must agree
   (a conflict is a diagnostic).
3. If the feature is reachable, invoke the provider with
   `{ module, symbols, options, datasets }`; otherwise drop the feature entirely
   (its prebuilt component and data are never linked).
4. Embed the returned `data` and wire it to the prebuilt data-free component
   (the `bdp-spike` composition). Cache key: `(module, sorted symbols, canonical
   options, dataset version, provider version)`.

Reachability is computed by the existing elaborator pass, which already does
reachability analysis and elimination. It is less exhaustive than a full DCE,
but sufficient here (over-keeping an op only over-bundles its data; correctness
is never affected). Piggy-backing on the elaborator also means the same usage
information can, with a small adjustment, be surfaced to the LSP. Because Wado
uses explicit named imports, the imported names are already a sufficient usage
signal; no separate whole-program call-graph pass is required. The one
intra-feature cost cliff — segmentation's multi-MB dictionary/LSTM data — is
gated by whether `words`/`lines` are imported.

Locale declarations are transitive and additive across the dependency graph: a
library that does `use ... with { locales: ["ja"] }` forces `ja` data into any
program that links it. This is correct (the library genuinely needs it) but must
be documented, since an application inherits locales it did not declare itself. A
future extension may add an application-level _kill switch_ to forcibly cap or
override the inherited locale/feature set; out of scope for v1.

### The `use ... with` surface

```wado
// text: data is not locale-partitioned; imported names select markers.
use { upper, fold } from "core:text";        // casemap markers (~24 KB)
use { normalize } from "core:text";           // normalizer markers (~157 KB)
use { category, script } from "core:text";    // properties markers

// collation: locales declared via `with`.
use { Collator } from "core:collation" with { locales: ["ja", "en-US"] };

// segmentation: the multi-MB data is opt-in by importing words/lines.
use { graphemes } from "core:segmentation";                                  // small
use { words, lines } from "core:segmentation" with { dictionaries: ["cjk"] }; // multi-MB
```

Locale option semantics:

- Omitted ⇒ root/`und` only (collation root UCA; no tailorings). Minimal by
  default.
- The declared set is the union across all use-sites; datagen expands it with
  likely-subtags and the fallback chain.
- A runtime langid outside the declared set falls back per ICU (to the nearest
  available, ultimately root). A _literal_ langid outside the set is a
  compile-time diagnostic.
- Only locale-bearing modules consume it (today, collation; later datetime /
  number formatting). It is inert elsewhere.

### ICU as the first consumer

ICU is special-cased only in that it is a first-party bundled consumer of the
general mechanism; it does not add compiler logic of its own.

- Components: a coarse split into `core:text` (casemap + normalizer +
  properties), `core:collation`, and `core:segmentation`. The split isolates the
  two heavy data sets (collation, segmentation) so unused features link nothing;
  within `core:text` the provider still slices data per imported op.
- Each component is a prebuilt, data-free Rust→wasm component (the `bdp-spike`
  model).
- The ICU provider is the ICU4X data slicer (`icu_provider_export` /
  `icu_provider_blob`) compiled to a wasm component. It slices the bundled image
  by `(symbols → markers)` and `(options.locales → locale set)`. Running from a
  bundled image needs no network, so it is fully deterministic and sandboxable.
- Collation's dependency on the normalizer's NFD markers (~37 KB, the one real
  cross-feature data dependency measured) is satisfied by including those markers
  in collation's own slice; no shared data component is needed at this size.

### Distribution: everything bundled

The full ICU distribution unit — prebuilt data-free components, the ICU data
image, the ICU provider component, and the `core:*` WIT surfaces — is bundled
with the toolchain (the `wado-bundled-libm` / `wado-bundled-icu` lineage), not a
userland package and not lazily fetched. `use { upper } from "core:text"` works
with no dependency to add. Programs that do not reference an ICU feature link
none of its code or data.

The data image is a single compressed, indexed archive (a zip-like container
with a central directory), with one entry per feature (and, where useful, per
marker or per locale). This keeps the `wado` compiler a single self-contained
binary while letting the host extract only the entries a build needs via the
`read-asset(name)` host import — so a casemap-only build never decompresses the
collation or segmentation entries. An indexed container (random access) is
preferred over a streamed one (e.g. tar) precisely for this selective
extraction. Postcard data compresses well, so the on-disk cost is far below the
raw figures above.

## Consequences

- Per-program data is minimal: only the markers for imported ops and the
  declared locales are embedded; unused features are absent entirely.
- Determinism improves over ad-hoc datagen: slicing reads a bundled image, never
  the network (the proxy/TLS non-determinism hit during the spike disappears).
- The compiler executes a wasm provider at compile time (already the Kiln
  execution model) and caches by content; repeat builds skip it.
- Toolchain size grows by the bundled ICU image. Accepted in exchange for the
  zero-dependency `core:` experience.
- Runtime-loaded data loses zero-copy-from-static and adds a fixed per-feature
  deserialization overhead; for a single feature this is roughly size-neutral
  versus baking. The win is across features and via per-program slicing.
- Data/code version coherence: the image, the prebuilt components, and the
  provider must share one pinned ICU/CLDR version, tied to the `core:*` package
  versions.

## TODO

Decided (folded into the design above):

- [x] Aggregation = per-(feature, program) union; options must be mergeable.
      Locale declarations are transitive; documented, with a future app-level
      kill switch as a possible extension.
- [x] op→marker map lives in the provider (compiler stays ICU-agnostic), guarded
      by a marker-recording test against ICU's constructors.
- [x] Reachability comes from the elaborator's existing reachability/elimination
      pass (sufficient here; also surfaces usage to the LSP), not a separate DCE.
- [x] Bundled image = a single compressed, indexed (zip-like) archive with
      per-feature/per-marker entries; selective extraction via `read-asset`,
      preserving the single-binary compiler.
- [x] Dynamic locale = ICU fallback to root by default; compile-time diagnostic
      for literal langids outside the declared set.
- [x] Host: generalize Kiln's `read-file` into a name-keyed `read-asset`; the
      data-provider reuses `core:kiln/kiln-host`, no separate host interface.

Remaining:

- [ ] Finalize the `data-provider` world WIT and the generalized `read-asset` on
      `core:kiln/kiln-host`.
- [ ] Pin down the elaborator hook: how the reachable import-edge set is handed
      to the provisioning step, and the LSP surface for it.
- [ ] Specify the option schema and its mergeability/canonical-encoding rules.
- [ ] Define the archive layout (entry granularity, index format, compression
      codec) and the slicing the provider does within an entry (markers ×
      locales).
- [ ] Build the provider's marker-recording drift test against ICU constructors.
- [x] Measure infra-code duplication across the three prebuilt components: the
      shared infra floor is ~10 KB/component (component glue + `BlobDataProvider`
      core), so the three-way split duplicates only ~20 KB total — negligible
      against the data. See `wado-bundled-icu/bdp-spike/infra-baseline/`.

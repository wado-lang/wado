# WEP: `core:icu` — Internationalization as One Facade

## Context

Internationalization is the standard library's largest capability and the one
least shaped by code. Unicode and CLDR are data: in the measured spike under
`wado-bundled-icu/`, an all-features ICU4X component is ~3.7 MB, of which
segmentation is ~2.35 MB and collation ~1.1 MB, against ~44 KB for the character
property tries. Once the surface widens past text into formatting, the CLDR
patterns for every locale dwarf even that.

Two questions follow. What does a program see, and how does it end up with only
the data it uses. This WEP answers the first and consumes
[compile-time data providers](./wep-2026-06-13-compile-time-data-providers.md)
for the second — `core:icu` is that mechanism's first consumer, and building it
is what proves the mechanism against a real slicer.

Feasibility is settled. The spike established that full ICU4X compiles to wasm
cleanly, that Rust LTO tree-shakes it by reachability so the WIT surface is the
size knob, and that a `no_std` build yields a zero-import self-contained
component. The `bdp-spike/` follow-up established the separation this design
rests on: a data-free component loading a sliced postcard blob at run time, with
the blob supplied either by the host or by a component composed in.

## Decision

### `core:icu` is the entire user-facing surface

One module. A program says `use icu from "core:icu"` and reaches every ICU
capability through it; nothing else is nameable and no dependency is added.

The facade is ordinary Wado source, organized into submodules and re-exported
through one entry. That matters more than namespacing convenience: since
[Provider Metadata](./wep-2026-07-26-provider-metadata.md) made the CM ABI stop
being the library contract, a `core:*` API is no longer obliged to be
CM-representable. The facade can therefore be Wado-native — generic,
trait-using, value-semantic — while the WIT beneath it is an internal ABI shaped
for the boundary rather than for users. Concretely, the facade is where ICU
capabilities acquire Wado shapes: a collator that satisfies `Ord`, segmentation
exposed through the iterator traits, `Display` and `Inspect` on locale
identifiers.

The user-visible module is deliberately not the unit of code splitting.

### Two API shapes, chosen by what the thing is

A free function, when the operation is a pure function of its arguments with no
construction worth keeping: normalization, case folding, `is_nfc`, character
properties, and one-shot locale-sensitive casing.

A resource, when the object is expensive to construct, is compiled from runtime
input, or is itself stateful: collators, the formatters (date/time, number, list,
relative time, message), plural rules, transliterators, and segmentation
iterators.

The test is whether the object is a pure function of a key the program bounds at
compile time. If it is, a free function costs nothing and keeps the value
semantics the rest of `core:*` has. If it is not, a handle is the honest model —
and across full ICU it frequently is not:

- Collator options are a combinatorial space (strength, alternate handling, case
  level, case first, numeric ordering, backward second level), and a tailored
  collator is compiled from user-supplied rules.
- A formatter is compiled from a skeleton or a message pattern, which is runtime
  data.
- A segmentation iterator over a large document is state by definition; forcing
  it into a `list<u32>` of every boundary is pathological when the caller wants
  the first few.

Where a capability serves both a one-shot and a loop, the facade offers both: a
convenience function that constructs and discards, and the resource for when the
construction should be hoisted. The facade makes the cost visible where it is
paid.

### The code partition is invisible

Underneath the facade sit several prebuilt, data-free implementation components,
partitioned so that an unused capability contributes no code. Users never name
them, and the partition can be re-cut without touching a single source file.

Which components a program links follows from reachability, not from what the
user imported: a capability with no live symbol is dropped whole, its code and
its data both absent.

### Reachability granularity

The facade carries the coarse split in its types. Where two operations differ in
data by orders of magnitude — a grapheme pass needs kilobytes where word
segmentation pulls a multi-megabyte dictionary — they belong to different types,
so naming a type is already a data decision and the cheap user never mentions the
expensive one. That works with reachability as it stands.

Method granularity is the refinement on top, for the residue that type splitting
would fragment absurdly. It is not available yet: the `liveness` pass seeds every
method as a live root by design, so a live type implies every one of its methods.
Depending on it before that changes would silently over-bundle, which is why the
type-level split is the load-bearing half.

### Locales are a compile-time declaration

```wado
use icu from "core:icu" with { locales: ["ja", "en-US"] };
```

- Omitted means root only — collation's root UCA, no tailorings. Minimal by
  default.
- The declared set is the union across all use sites. The provider expands it
  with likely-subtags and the fallback chain.
- A runtime langid outside the set falls back per ICU, to the nearest available
  and ultimately to root. A literal langid outside the set is a compile-time
  diagnostic.
- The declaration is transitive and additive across the dependency graph: a
  library declaring `ja` forces `ja` into any program linking it. This is correct
  — the library genuinely needs it — but an application inherits locales it never
  declared. A later extension may add an application-level cap; out of scope
  here.

A single facade means one declaration site per file rather than one per
capability, so the union has one obvious place to look.

### The ICU package

`core:icu` is a provider-backed package in the sense the data-provider WEP
defines, with one difference: it ships with the toolchain rather than being
fetched, so `use icu from "core:icu"` works with nothing to add. That changes
where the package comes from and nothing about how it works.

Its four parts:

- The facade — the Wado source above.
- Implementation components — the spike's data-free components, promoted to
  first-party artifacts.
- The provider — the ICU4X data slicer compiled to wasm. It maps symbols to
  markers and the declared locales to a locale set, then re-exports the selected
  markers through ICU's blob exporter, reading the bundled image instead of CLDR.
  Running from a bundled image needs no network, which is what makes it
  deterministic and sandboxable — and removes the proxy and TLS non-determinism
  the spike hit against live CLDR downloads.
- Data assets — the image below.

The symbol-to-marker map lives entirely in the provider, because it is
ICU-version-specific: a collator's constructor also pulls the normalizer's NFD
markers, which no crate-level marker list mentions. Keeping it there is what lets
the compiler stay ICU-agnostic, passing only Wado symbol names.

That map is guarded by a recording test rather than by review. A buffer-provider
wrapper logs every marker each constructor actually requests, so the map is
derived from observed behaviour instead of documentation. The recorded set is
request-specific — root collation pulls no tailoring markers — so the map is the
union over inputs chosen to exercise every path, and the test asserts coverage
and fails on drift when ICU is upgraded.

### The data image

One compressed, indexed archive, which keeps `wado` a self-contained binary while
letting the provider extract only what a build needs.

- Entries are per-marker. That granularity is what lets a grapheme-only build
  never touch segmentation's dictionary and LSTM entries, while keeping the index
  small — per-(marker, locale) would explode it for collation. A locale-bearing
  marker's entry holds every locale; the provider slices within it.
- The index maps entry name to offset and length. Random access is required for
  selective extraction, so a streamed archive format does not qualify.
- Compression is per-entry zlib, reversed host-side, so the provider links no
  decompressor. zlib is chosen over zstd deliberately: it is already a compiler
  dependency and pure Rust, where zstd would add a C dependency to an otherwise
  runtime-free compiler. Measured on the ICU blobs, zstd-19 beats zlib-9 by ~9%
  on the dominant, near-incompressible segmentation data and ~8% on the small
  feature blobs, with its best case ~24% on mid-size locale data — not enough to
  buy the dependency. Compression happens once at toolchain-build so its speed is
  irrelevant; decompression is ~2–3× faster with zstd but already negligible (the
  largest 4 MB entry takes ~24 ms, and a build pulls few entries). zlib runs at
  its default level: higher levels bought no measurable ratio on this data.

Collation's dependency on the normalizer's NFD markers — ~37 KB, the one real
cross-capability data dependency measured — is satisfied by including them in
collation's own slice. At that size a shared data component is not worth its
composition cost.

## Alternatives considered

### A resource-free surface keyed by (locale, options)

Every ICU object could be a pure function of a key, with the component memoizing
internally: `compare(locale, options, a, b)` instead of a constructed collator.
Attractive because it keeps the surface value-semantic and needs no resource
support in the CM import path. Rejected on four grounds.

The far-side memo is a cache with no eviction policy and no visibility. Compile
time bounds the locale _data_, not the _keys_ — a service reading
`Accept-Language` sees unbounded key strings, and the program cannot inspect or
bound what accumulates.

It hides cost. A constructor says where the expensive thing happens; a call that
is expensive only the first time reads as free at every call site, and a profile
of the hot comparison cannot show the construction inside it.

The key is not small. Collator options alone are combinatorial, and a tailored
collator is compiled from user rules — not a key at all.

Most decisively, it does not scale to the full surface. Formatters compiled from
skeletons and message patterns, and segmentation iterators, are functions of no
compile-time-bounded key. A design that holds for case mapping and breaks at date
formatting is not a design for `core:icu`.

The counterargument — that move-only resources clash with Wado's value semantics
— is real but narrow. It bites only when a resource is treated as a copyable
value, which none of these need to be, and Wado users already meet resources
throughout the WASI standard library.

### Three user-visible modules

An earlier plan split the surface into `core:text`, `core:collation`, and
`core:segmentation` so that unused capabilities linked nothing. Rejected: it
conflated a code partition with a naming decision. Data slicing is per-symbol
regardless of module and reachability already selects components, so the split
bought nothing reachability does not — while costing users a memorized map from
capability to module, spreading the locale declaration across three `use` sites
to be unioned, and freezing an internal partition into the public API where
re-cutting it becomes a breaking change.

### Baking the data into each component

ICU4X's compiled-data mode, which the original spike used. Simpler, and needs no
provider at all, but it is ~3.7 MB for the full text surface and cannot slice by
locale — so a program declaring two locales still carries every locale CLDR
ships. That is survivable for casing and normalization and hopeless for
formatting, which is where the surface is going.

### A shared data component

Data common to several capabilities could live in one component the others
import. Measured, the sharing is not there: the collator/normalizer overlap is
~37 KB and the casemap, properties, and segmenter sets overlap by essentially
nothing, against a ~10 KB floor per additional component. Dedup follows genuine
runtime data dependencies, not taxonomy.

## Consequences

- Users learn one module and no partition. The capability-to-component map is an
  implementation detail that can change without a source change.
- Per-program data is minimal: only the markers for live symbols and the declared
  locales are embedded, and an unreachable capability contributes nothing.
- The API tells the truth about cost. A constructed object is visibly
  constructed, and the one-shot helpers beside it are visibly one-shot.
- Toolchain size grows by the bundled ICU package, in exchange for a
  zero-dependency `core:` experience.
- Runtime-loaded data loses zero-copy-from-static and adds a fixed per-capability
  deserialization cost. For a single capability this is roughly size-neutral
  against baking; the win is across capabilities and from per-program slicing.
- Resources in the surface make this depend on CM resource import, which is not
  implemented. The cost is shared: the same gap blocks every resource-bearing
  third-party component, and WASI's own surface is resource-heavy.

## Open questions

- [ ] Can a resource be held in a global? An HTTP service wants one collator
      across requests, so the answer shapes how usable the resource form is.
- [ ] Can a borrow be captured by a closure — `list.sort_by(|a, b| c.compare(a, b))`
      — under the ownership analysis
      ([resource ownership](./wep-2026-05-21-resource-ownership.md))? If not, the
      sorting path needs a different shape.
- [ ] Does ICU4X expose collation sort keys? A bulk key-extraction call would let
      a sort cross the boundary once instead of per comparison, which dominates
      any handle-versus-lookup difference.
- [ ] Where the facade's date/time formatting meets
      [`core:temporal`](./wep-2026-06-05-core-temporal.md), and how its
      time-zone view relates to the WASI-provided one.
- [ ] The full capability inventory and its component partition. The first cut
      covers what the spikes measured — locale identity, normalization, case
      mapping, character properties, segmentation, collation — with formatting,
      plural rules, and transliteration following.

## Implementation

- [ ] Resource and handle support in CM component import
      ([Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)).
      A prerequisite: the facade's resource-bearing components cannot be consumed
      without it.
- [ ] The [data-provider mechanism](./wep-2026-06-13-compile-time-data-providers.md)
      itself.
- [ ] Promote the spike's data-free components into first-party prebuilt
      artifacts with their WIT surfaces, partitioned per the inventory.
- [ ] Write the facade over them: re-exports, the Wado-native shapes (`Ord`,
      iterators, `Display`), and the one-shot helpers.
- [ ] Build the ICU provider with its symbol-to-marker map and the
      marker-recording drift test.
- [ ] Build the data image — per-marker zlib entries plus index — and bundle the
      package with the toolchain.

## References

- [Compile-Time Data Providers](./wep-2026-06-13-compile-time-data-providers.md)
  — the mechanism this consumes.
- [research: splitting large libraries](./research-library-splitting.md) — the
  measurements behind the partition decisions.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) — how
  the implementation components are consumed and composed.
- [Provider Metadata](./wep-2026-07-26-provider-metadata.md) — why a `core:*` API
  need no longer be CM-representable.
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — the discipline
  the facade's resources carry.

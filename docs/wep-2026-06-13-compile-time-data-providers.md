# WEP: Bundled ICU — the `core:icu` Facade and Compile-Time Data Providers

## Context

Internationalization is the standard library's largest capability and the one
least shaped by code. Unicode and CLDR are data: in the measured spike under
`wado-bundled-icu/`, an all-features ICU4X component is ~3.7 MB, of which
segmentation is ~2.35 MB and collation ~1.1 MB, against ~44 KB for the character
property tries. Baking that into every program is untenable, and so is asking
users to hand-pick data sets.

Two questions follow, and this WEP answers both:

- What does a Wado program see? One module, `core:icu`, covering the whole ICU
  surface.
- How does a program end up with only the data it uses? A general compile-time
  data provider mechanism, of which ICU is the first consumer.

The feasibility work is done. The spike established that full ICU4X compiles to
wasm cleanly, that Rust LTO tree-shakes it by reachability so the WIT surface is
the size knob, and that a `no_std` build yields a zero-import self-contained
component. The `bdp-spike/` follow-up established the separation this design
rests on: a data-free feature component loading a sliced postcard blob at run
time, with the blob supplied either by the host or by a data component composed
in over the Component Model.

### Why not Kiln

[Kiln](./wep-2026-04-12-kiln.md) already provides the right plumbing —
sandboxed, deterministic, content-addressed compile-time generators invoked from
`use ... with` sites. But Kiln is deliberately narrowed to "IDL → Wado source":
its generators are Wado packages, its input is a user schema file, and its output
is `.wado` source. Data provisioning needs three things Kiln intentionally does
not do:

- Emit a binary data asset and wire it to a prebuilt component, not source.
- Be driven by which exported symbols are actually used, not by a schema file.
- Run a provider authored in any language, since ICU's slicer is ICU4X and the
  host only ever executes an opaque wasm component.

So this is a sibling mechanism reusing Kiln's infrastructure — sandbox, cache,
host-delegated execution — under a different contract, rather than a widening of
Kiln's charter.

## Decision

### `core:icu` is the entire user-facing surface

One module. A program says `use icu from "core:icu"` and reaches every ICU
capability through it; nothing else is nameable and no dependency is added.

The facade is ordinary Wado source, organized into submodules and re-exported
through one entry. That matters more than namespacing convenience: since
[Provider Metadata](./wep-2026-07-26-provider-metadata.md) made the CM ABI stop
being the library contract, a `core:*` API is no longer obliged to be
CM-representable. The facade can therefore be Wado-native — generic, trait-using,
value-semantic — while the WIT it talks to underneath is an internal ABI designed
for the boundary rather than for users. Concretely, the facade is where ICU
capabilities are given Wado shapes: a collator that satisfies `Ord`, segmentation
exposed through the iterator traits, `Display` and `Inspect` on locale
identifiers.

The user-visible module is deliberately not the unit of code splitting. See
"The code partition is invisible".

### Two API shapes, chosen by what the thing is

An ICU capability surfaces in one of two forms.

A free function, when the operation is a pure function of its arguments with no
construction worth keeping: normalization, case folding, `is_nfc`, character
properties, and one-shot locale-sensitive casing.

A resource, when the object is expensive to construct, is compiled from runtime
input, or is itself stateful: collators, the formatters (date/time, number,
list, relative time, message), plural rules, transliterators, and segmentation
iterators.

The test is whether the object is a pure function of a key that the program
bounds at compile time. If it is, a free function costs nothing and keeps the
value semantics the rest of `core:*` has. If it is not, a handle is the honest
model — and across full ICU, it frequently is not:

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
construction should be hoisted. The facade makes the cost visible at the point
where it is paid.

### The code partition is invisible

Underneath the facade sit several prebuilt, data-free components — the
`bdp-spike` model — partitioned so that an unused capability contributes no code.
Users never name them, and the partition can be re-cut without touching a single
source file.

Which components a program links is derived from reachability, not from what the
user imported. A capability with no live symbol is dropped whole: its component
and its data are never linked.

This is why one facade does not resurrect the size problem the earlier
three-module split was meant to solve. Data slicing was always per-symbol, never
per-module; the module split only ever bought a code partition, and reachability
buys the same thing without exposing it.

### Reachability granularity: symbols, and methods on resources

The unit of reachability is the imported symbol, and for a resource, the method.

The facade carries the coarse split in its types. Where two operations differ in
data by orders of magnitude — a grapheme pass needs kilobytes where word
segmentation pulls a multi-megabyte dictionary — they belong to different types,
so naming a type is already a data decision and the cheap user never mentions the
expensive one. That much works with reachability as it stands.

Method granularity is the refinement on top, for the residue that type splitting
would fragment absurdly. It is not available yet: the liveness pass seeds every
method as a live root by design, so today a live type implies every one of its
methods. Depending on it before that changes would silently over-bundle, which is
why the type-level split is the load-bearing half and method granularity is
tracked as work.

Over-keeping is safe either way: it over-bundles data, never changes behaviour.

### Locales are a compile-time declaration

Locale-bearing capabilities take their locale set from the `use` site:

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
  — the library genuinely needs it — but it means an application inherits locales
  it never declared. A later extension may add an application-level cap; out of
  scope here.

With a single facade there is one declaration site per file rather than one per
capability module, so the union has one obvious place to look.

### The provider mechanism

A data provider is a wasm component that the compiler invokes during
compilation, sandboxed and cached exactly like a Kiln generator, but with a
contract specialized for data slicing. Its source language is unconstrained: the
contract is defined over the component, not over Wado source.

Surface and data are separated. The type surface of a provider-backed module is
prebuilt and bundled, so type-checking resolves names with no provider
involvement — it works offline and it is fast. The provider produces only data,
and runs once reachability is known.

```wit
package core:provider;

interface types {
  record request {
    module: string,         // "core:icu"
    symbols: list<string>,  // live imported names, methods qualified
  }
  record blob {
    component: string,      // which prebuilt component this data belongs to
    data: list<u8>,
  }
  record response {
    blobs: list<blob>,
  }
}

interface provider-host {
  /// Read a bundled compiler asset — one entry of the ICU data archive — as
  /// raw bytes, resolved in the toolchain's bundled namespace. Calls are
  /// recorded and contribute to the cache key.
  read-asset: func(name: string) -> result<list<u8>, host-error>;

  /// Report a diagnostic, surfaced as an ordinary compile diagnostic.
  emit-diagnostic: func(diagnostic: diagnostic);
}

world data-provider {
  import provider-host;
  use types.{request, response};
  export provide: func(req: request) -> response;
}
```

One invocation per (provider-backed module, program). Because the facade is a
single module, ICU is one invocation returning one blob per surviving component.

The `with { ... }` options a use site declares are absent from the sketch on
purpose; see "Options".

The host interface is the provider's own, not Kiln's. It mirrors Kiln's
diagnostic shapes so the two report identically, but `core:kiln/kiln-host` is
left untouched: adding `read-asset` there would hand every Kiln generator a read
into the toolchain's asset namespace it has no reason to hold, widening a sandbox
whose narrowness is the point. The two capability sets are disjoint by intent — a
generator reads user files at its declaration site and never bundled assets, a
provider reads bundled assets and never user files.

Diagnostics go through `emit-diagnostic`, so the response carries only data. The
request has no dataset list: the provider pulls the entries it needs via
`read-asset`, and those recorded reads join the cache key — mirroring how Kiln
records its own reads.

The symbol→marker mapping lives entirely in the provider. It is
ICU-version-specific (a collator's constructor also pulls the normalizer's NFD
markers), so keeping it there leaves the compiler ICU-agnostic, passing only Wado
symbol names. Correctness is guarded by a recording test: a buffer-provider
wrapper logs every marker each constructor requests, so the map is derived from
observed behaviour rather than from crate-level marker lists, which omit
transitive dependencies. The recorded set is request-specific — root collation
pulls no tailoring markers — so the map is the union over inputs chosen to
exercise every path, and the test asserts coverage and flags drift on ICU
upgrades.

### Compiler aggregation

1. Resolve `core:icu` as provider-backed and type-check against the bundled
   surface.
2. Collect every `use` site across the program: the union of live symbols and the
   merged options.
3. If any symbol survives, invoke the provider; otherwise drop the module
   entirely.
4. Embed each returned blob and wire it to its prebuilt component, composing the
   result. Cache key: the module, the sorted symbols, the canonical options, the
   provider's source hash, and the recorded asset reads.

Reachability comes from the elaborator's existing `liveness` pass
([elaborator rearchitecture](./wep-2026-05-26-elaborator-rearchitecture.md)),
which already computes the closure of items reachable from the export boundary
and already feeds both `reify` and the
[unused-import diagnostics](./wep-2026-05-16-unused-diagnostics.md). Data
provisioning is a third consumer of the same result: take the live imported
symbols by their Wado names and hand them to the provider. For a free function
this needs nothing new — Wado's explicit named imports make the imported name
itself the usage signal, so no whole-program call-graph pass is added.

Methods are the part that pass does not answer today. It classifies free
functions and globals, and seeds every method as a production root so that no
method is ever reported dead — a deliberate soundness choice that defers
method-level detection to the follow-up slice its own design names. Until that
lands, a live type means all of its methods are live, so the facade's type-level
split carries the whole burden and a program touching only cheap methods of an
expensive type over-bundles. That is the cost of the current pass, not of this
design; see the implementation list.

Because that pass also backs the unused-import diagnostics, the LSP knows
"imported but unused" for ICU symbols with no extra work. Surfacing the data cost
of a live import — that word segmentation pulls megabytes — is an inlay hint
built on the provider's size metadata, deferred past v1.

### Options

A provider-backed module's `with { ... }` options are typed against a declaration
on its bundled surface, so the compiler validates a use site before any provider
runs. How that declaration is written, and how the validated value reaches the
provider, is unsettled. Kiln solves the same problem for generator options but
not in a form this mechanism can adopt verbatim: a Kiln generator carries its
options as a typed argument in a world it synthesizes for itself, whereas
provider-backed modules all conform to one `data-provider` world. Decide it when
the mechanism is implemented.

What is settled is the aggregation, which does not depend on how options are
declared or carried. Merging across use sites is by type:

- `list<T>` options merge by set-union, deduplicated and order-normalized —
  `locales`, and segmentation's dictionary set.
- Scalar options must agree across use sites; a conflict is a diagnostic.

This type-driven rule covers every ICU option, so v1 adds no per-field merge
annotations. Introducing them is a later extension only if a future provider
needs it. Note that a collator's runtime fallback mode is a property of the
constructed object, not of data slicing, so it is an ordinary constructor
argument rather than a `with` option.

### Distribution: everything bundled

The full ICU distribution unit — the prebuilt data-free components, the ICU data
image, the ICU provider component, and the facade's own WIT surfaces — ships with
the toolchain, in the `wado-bundled-libm` / `wado-bundled-icu` lineage. It is not
a userland package and it is not lazily fetched, so `use icu from "core:icu"`
works with nothing to add. A program that references no ICU capability links none
of its code and none of its data.

The data image is one compressed, indexed archive embedded in the toolchain,
keeping `wado` a self-contained binary while letting the host extract only the
entries a build needs.

- Entries are per-marker. That granularity is what lets a grapheme-only build
  never touch segmentation's dictionary and LSTM entries, while keeping the index
  small — per-(marker, locale) would explode it for collation. A locale-bearing
  marker's entry holds every locale; the provider slices within it.
- The index is a minimal central directory mapping entry name to offset and
  length. Random access is required for selective extraction, so a streamed
  archive format does not qualify.
- Compression is per-entry zlib, applied and reversed host-side, so `read-asset`
  returns decompressed bytes and the provider links no decompressor. zlib is
  chosen over zstd deliberately: it is already a compiler dependency and pure
  Rust, where zstd would add a C dependency to an otherwise runtime-free
  compiler. Measured on the ICU blobs, zstd-19 beats zlib-9 by ~9% on the
  dominant, near-incompressible segmentation data and ~8% on the small feature
  blobs, with its best case ~24% on mid-size locale data — not enough to buy the
  dependency. Compression happens once at toolchain-build so its speed is
  irrelevant; decompression is ~2–3× faster with zstd but already negligible
  (the largest 4 MB entry takes ~24 ms, and a build pulls few entries). zlib runs
  at its default level: higher levels bought no measurable ratio on this data.

Given the entries its symbols need plus the locale set, the provider produces
each component's blob by re-exporting the selected markers and locales through
ICU's blob exporter — the same machinery as offline datagen, reading the bundled
image instead of CLDR.

Collation's dependency on the normalizer's NFD markers — ~37 KB, the one real
cross-capability data dependency measured — is satisfied by including them in
collation's own slice. At that size a shared data component is not worth its
composition cost.

## Alternatives considered

### A resource-free surface keyed by (locale, options)

Every ICU object could be treated as a pure function of a key, with the component
memoizing internally: `compare(locale, options, a, b)` instead of a constructed
collator. This is attractive because it keeps the whole surface value-semantic
and needs no resource support in the CM import path. It was rejected on four
grounds.

The far-side memo is a cache with no eviction policy and no visibility. Compile
time bounds the locale _data_, not the _keys_ — a service reading
`Accept-Language` sees unbounded key strings, and the program has no way to
inspect or bound what accumulates.

It hides cost. A constructor says where the expensive thing happens; a call that
is expensive only the first time reads as free at every call site, and a profile
of the hot comparison cannot show the construction hiding inside it.

The key is not small. Collator options alone are combinatorial, and a tailored
collator is compiled from user rules — not a key at all.

Most decisively, it does not scale to the full surface this WEP commits to.
Formatters compiled from skeletons and message patterns, and segmentation
iterators, are not functions of any compile-time-bounded key. A design that holds
for case mapping and breaks at date formatting is not a design for `core:icu`.

The counterargument — that move-only resources clash with Wado's value semantics
— is real but narrow. It bites only when a resource is treated as a copyable
value, which none of these need to be, and Wado users already meet resources
throughout the WASI standard library.

### Three user-visible modules

The earlier plan split the surface into `core:text`, `core:collation`, and
`core:segmentation`, so that unused capabilities linked nothing. Rejected: the
split conflated a code partition with a naming decision. Data slicing is
per-symbol regardless of module, and reachability already determines which
components link, so the split bought nothing that reachability does not — while
costing users a memorized map from capability to module, spreading the locale
declaration across three `use` sites that must then be unioned, and freezing an
internal partition into the public API where re-cutting it becomes a breaking
change.

### Baking data into each component

The original spike used ICU4X's compiled-data mode, where each component carries
its own tables. It is simpler and needs no provider, but it is ~3.7 MB for the
full surface and cannot slice by locale at all. The measured per-capability
attribution — segmentation and collation together are ~93% of the bytes — is what
makes per-program slicing worth a compile-time mechanism.

### A shared data component

Data common to several capabilities could live in one component that the others
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
  constructed, and the one-shot helpers next to it are visibly one-shot.
- Determinism improves over ad-hoc datagen: slicing reads a bundled image and
  never the network, which removes the proxy and TLS non-determinism the spike
  hit.
- The compiler executes a wasm provider at compile time — already the Kiln
  execution model — and caches by content, so repeat builds skip it.
- Toolchain size grows by the bundled ICU image, in exchange for a
  zero-dependency `core:` experience.
- Runtime-loaded data loses zero-copy-from-static and adds a fixed per-capability
  deserialization cost. For a single capability this is roughly size-neutral
  against baking; the win is across capabilities and from per-program slicing.
- The image, the prebuilt components, and the provider must share one pinned
  ICU/CLDR version, tied to the facade's package version.
- Resources in the surface make this WEP depend on CM resource import, which is
  not implemented. That is a cost, but a shared one: the same gap blocks every
  resource-bearing third-party component, and WASI's own surface is resource-heavy.

## Open questions

- [ ] Can a resource be held in a global? An HTTP service wants one collator
      across requests, so the answer shapes how usable the resource form is.
- [ ] Can a borrow be captured by a closure — `list.sort_by(|a, b| c.compare(a, b))`
      — under the ownership analysis
      ([resource ownership](./wep-2026-05-21-resource-ownership.md))? If not, the
      sorting path needs a different shape.
- [ ] Does ICU4X expose collation sort keys? If so, a bulk key-extraction call
      lets a sort cross the boundary once instead of per comparison, which
      dominates any handle-versus-lookup difference.
- [ ] Where the facade's date/time formatting meets
      [`core:temporal`](./wep-2026-06-05-core-temporal.md), and how its time-zone
      view relates to the WASI-provided one.
- [ ] The full capability inventory and its component partition. The first cut
      covers what the spikes measured — locale identity, normalization, case
      mapping, character properties, segmentation, collation — with formatting,
      plural rules, and transliteration following.

## Implementation

The design rests on spikes under `wado-bundled-icu/`: data-free components
running on runtime-loaded and composed-in blobs, the collator-to-normalizer
marker dedup, the near-zero dedup elsewhere, the shared-infra floor, the
marker-recording mechanism, and the zlib-versus-zstd comparison.

- [ ] Resource and handle support in CM component import
      ([Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)).
      A prerequisite: the facade's resource-bearing components cannot be consumed
      without it.
- [ ] Settle the options protocol: how a provider-backed module declares its
      `with { ... }` options, and how the validated value crosses to a provider
      conforming to the shared `data-provider` world.
- [ ] Promote the spike's data-free components into first-party prebuilt
      artifacts with their WIT surfaces, partitioned per the inventory above.
- [ ] Write the `core:icu` facade over them: re-exports, the Wado-native shapes
      (`Ord`, iterators, `Display`), and the one-shot helpers.
- [ ] Build the ICU provider component with its symbol→marker map and the
      marker-recording drift test.
- [ ] Build the bundled archive — per-marker zlib entries plus index — and the
      provider host interface carrying `read-asset`, separate from
      `core:kiln/kiln-host`.
- [ ] Extend the `liveness` pass to method-level reachability, so a live type
      stops implying every method. Until then the facade's type-level split is
      the only granularity that holds, and expensive methods on a live type are
      bundled whether or not they are called.
- [ ] Wire the provisioning phase: aggregate live symbols and options off the
      `liveness` pass, invoke the provider, embed and compose the result, and
      cache by content.

## References

- [research: splitting large libraries](./research-library-splitting.md) — the
  levers and hard constraints this design is built on.
- [Kiln](./wep-2026-04-12-kiln.md) — the execution and caching infrastructure
  reused here.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) — how
  the prebuilt components are consumed and composed.
- [Provider Metadata](./wep-2026-07-26-provider-metadata.md) — why a `core:*` API
  need no longer be CM-representable.
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — the discipline
  the facade's resources carry.

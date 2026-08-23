# WEP: `core:icu` — Internationalization as One Facade

## Context

Internationalization is the standard library's largest capability and the one
least shaped by code. Unicode and CLDR are data, and the data separates along
two axes that behave nothing alike.

By capability: in the `wado-bundled-icu/` spike an all-features ICU4X component
is ~3.7 MB, of which segmentation is ~2.35 MB and collation ~1.12 MB, against
~44 KB for the character property tries.

By locale, measured with `bdp-spike/datagen` against icu4x 2.2 and CLDR 48.2.0:

| markers                                       | root (`und`) | `und,ja` | every locale |
| --------------------------------------------- | -----------: | -------: | -----------: |
| collation                                     |       568 KB |   660 KB |      1043 KB |
| formatting (date/time, number, list, plurals) |        21 KB |    47 KB |      4334 KB |

The two rows invert. Collation is a root table with tailorings hung off it, so
slicing by locale saves at most 475 KB. Formatting is almost nothing but locale
data, at a 206× ratio — and that is why this design exists. No amount of
dead-code elimination substitutes: a locale is reached through the same
constructor whatever the program declares.

Two questions follow: what a program sees, and how it ends up with only the data
it uses. This WEP answers both, and the second answer is a lookup in a bundled
archive rather than a slicing run — most of what
[compile-time data providers](./wep-2026-06-13-compile-time-data-providers.md)
offers goes unused here.

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

### Three API shapes, chosen by what the thing is

| Shape                                | When                                                               |
| ------------------------------------ | ------------------------------------------------------------------ |
| Free function                        | pure in its arguments, no construction worth keeping               |
| Constructed handle, non-owning token | construction worth hoisting, configuration bounded at compile time |
| Constructed handle, affine resource  | built from unbounded runtime input, or stateful                    |

A free function covers normalization, case folding, `is_nfc`, character
properties, and one-shot locale-sensitive casing.

What separates the two handle shapes is who ends the object's life. It is not
the line the data slicing draws — that one is capability and locale, and it runs
across both shapes.

Statefulness forces the affine shape for a second, independent reason: copying a
token aliases its referent, which two copies of a lazy iterator would show by
advancing each other. So the segmenter is a token and the iteration is not — a
full pass returns a `List<u32>` of boundaries, while lazy iteration is affine, an
eager list being pathological when the caller wants the first few.

Where a capability serves both a one-shot and a loop, the facade offers both: a
convenience function that constructs and discards, and the handle for when the
construction should be hoisted. The facade makes the cost visible where it is
paid.

#### Non-owning token

A collator over a declared locale, plural rules, a formatter over a fixed
skeleton, a segmenter: immutable, and configured only from what
`with { locales: [...] }` and the program's types already bound. The
implementation component interns these, so the set is finite by construction and
never needs freeing, and the handle keeps the value semantics the rest of
`core:*` has — it sits in a global, and a closure captures it by copy, so
`list.sort_by(|a, b| c.compare(a, b))` is written the obvious way.

#### Affine resource

A collator tailored from user-supplied rules, a formatter compiled from a runtime
skeleton or message pattern, anything keyed by an `Accept-Language` string, a
lazy segmentation iterator. The far side allocates these per call from input the
program does not bound, so the object has an end and something must reach it: the
handle carries a `dtor` and is move-only.

### The capability axis is DCE's, the locale axis is the package's

Nothing in `core:icu` decides which capabilities a program links: reachability
does, and `wado-wasm-embed` drops what the exports it keeps cannot reach. What
that prune cannot reach is baked data (Known gaps), which is why the
implementation components carry none and the package supplies it instead.

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
so naming a type selects a component and the cheap user never mentions the
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

Its three parts:

- The facade — the Wado source above.
- Implementation components — the spike's data-free components, promoted to
  first-party artifacts. Each carries the marker set it loads, fixed when the
  component is built.
- Data assets — the image below, keyed by (component, locale).

Assembling a program's data is a lookup: the live components name their marker
sets, the declared locales expand through likely-subtags and the fallback chain,
and the entries at those keys are handed over as blobs. ICU4X's fork provider
lets one component read several blobs, so nothing merges or re-encodes them.

Marker granularity finer than the component is deliberately not pursued. For a
program declaring `ja` and formatting dates, the locale declaration is worth
4.3 MB where choosing within datetime's 57 markers is worth tens of KB inside
the 47 KB that remains. Fixing the marker set per component is what removes the
ICU-version-specific symbol-to-marker map, and with it the recording test that
would have had to catch the map drifting on every ICU upgrade.

### The data image

One compressed, indexed archive, which keeps `wado` a self-contained binary while
letting the provider extract only what a build needs.

- Entries are per (component, locale), and one per component where the data is
  locale-independent. An entry is exactly what one component loads for one
  locale, so a build concatenates entries instead of slicing inside them. The
  index stays small because components are tens, where a per-(marker, locale)
  key would run to tens of thousands.
- The index maps entry name to offset and length. Random access is required for
  selective extraction, so a streamed archive format does not qualify.
- A locale's entry is stored deduplicated against its fallback parent, which the
  runtime fallbacker undoes. Measured, that is 5.16 MB down to 4.34 MB on
  formatting and 23 bytes on collation — CLDR patterns inherit heavily where
  collation tailorings are genuinely distinct.
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
— is real, and the three-shape split above is the answer to it rather than a
concession: the objects that want to be copyable values are copyable values.
What stays move-only is what genuinely has an end.

### One handle shape for everything

Making every handle affine adds no kind, but move-only on an immutable interned
object is a discipline with no safety payoff — nothing can be double-freed
because nothing is freed — and it costs what the hot paths need: a collator in a
global, and a collator captured by a comparison closure. It would also put the
entire surface behind CM resource import rather than the part owning a `dtor`.

Making every handle a bare index is the mirror error. It reintroduces what sank
the memoizing surface above — an object built per request from `Accept-Language`
accumulates far-side with no eviction and no visibility, a leak rather than a
cache — and it silently shares the state of every object that has some.
Representation was never the question: a CM handle is already an index into a
per-instance table.

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
data assets at all, but it cannot slice by locale — a program declaring two
locales still carries every locale CLDR ships. Survivable for casing and
normalization; for formatting it is 4.34 MB where the declared locales need
47 KB.

### A shared data component

Data common to several capabilities could live in one component the others
import. Measured, the sharing is not there: the collator/normalizer overlap is
~37 KB and the casemap, properties, and segmenter sets overlap by essentially
nothing, against a ~10 KB floor per additional component. Dedup follows genuine
runtime data dependencies, not taxonomy.

## Consequences

- Users learn one module and no partition. The capability-to-component map is an
  implementation detail that can change without a source change.
- Per-program data is bounded by the live components and the declared locales;
  an unreachable capability contributes nothing.
- The API tells the truth about cost. A constructed object is visibly
  constructed, and the one-shot helpers beside it are visibly one-shot.
- Toolchain size grows by the bundled ICU package, in exchange for a
  zero-dependency `core:` experience.
- Runtime-loaded data loses zero-copy-from-static and adds a fixed per-capability
  deserialization cost. For a single capability this is roughly size-neutral
  against baking; the win is the locale axis, which baking cannot reach at all.
- No ICU code runs at build time, so the build has nothing to sandbox, cache by
  content, or keep deterministic beyond reading its own archive.
- The CM import blocker splits. The compile-time-bounded surface needs only a
  `dtor`-less imported handle; the tailored, runtime-configured, and stateful
  surface waits on full resource import, a gap shared with every
  resource-bearing third-party component.

## Known gaps

- **The prune keeps baked data.** `wado-wasm-embed` drops every function,
  global, table, tag, type and segment unreachable from the exports it keeps,
  but marks every active data segment live unconditionally: an active segment
  initialises memory whether or not anything still reads what it wrote.

  Closing it means giving the prune a symbol graph where it has a wasm index
  graph. The `linking` and `reloc.*` sections carry exactly that — which
  function references which data symbol — so an asset kept relocatable could be
  collected from the live exports the way a linker's `--gc-sections` does. Worth
  ~2.35 MB where one component serves both a grapheme pass and word
  segmentation and a program reaches only the first, and it would let a
  component bake its locale-independent data again. Unverified: whether those
  sections survive the ICU asset's build with the edges intact.

## Open questions

- [ ] What bounds a token's interned set. Tokens forbid eviction — an entry
      freed under a live token dangles — so the set is a permanent high-water
      mark and must be small, not merely finite. The bound wanted is on the key
      type's cardinality, which is a facade-authoring decision rather than
      anything the compiler checks; whether it should instead be a distinct type
      per bounded configuration is undecided. Unrelated to data slicing.
- [ ] Holding an affine handle in a global, and capturing one in a closure
      ([resource ownership](./wep-2026-05-21-resource-ownership.md)). The token
      form needs neither, so this bounds only the tailored, runtime-configured,
      and stateful surface — where a service caching a collator per
      `Accept-Language` lives.
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

- [ ] Non-owning token support in CM component import
      ([Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)):
      a `dtor`-less imported handle decoded as a copyable newtype. It is all the
      first slice needs — normalization, case mapping, character properties,
      collation over the declared locales, and grapheme boundaries by eager
      pass.
- [ ] Affine resource support in the same path — methods, static constructors,
      `borrow<T>` parameters, `resource.drop` — gating the tailored,
      runtime-configured, and stateful surface, not the package.
- [ ] The [data-provider mechanism](./wep-2026-06-13-compile-time-data-providers.md)
      itself, reduced here to an archive lookup: `core:icu` runs no provider
      component and needs no compile-time execution.
- [ ] Promote the spike's data-free components into first-party prebuilt
      artifacts with their WIT surfaces, partitioned per the inventory.
- [ ] Write the facade over them: re-exports, the Wado-native shapes (`Ord`,
      iterators, `Display`), and the one-shot helpers.
- [ ] Build the data image — per-(component, locale) zlib entries plus index,
      deduplicated against the fallback parent — and bundle it with the
      toolchain.
- [ ] Feed several blobs to one component through ICU4X's fork provider.

## References

- [Compile-Time Data Providers](./wep-2026-06-13-compile-time-data-providers.md)
  — the mechanism this consumes, here reduced to an archive lookup.
- [research: splitting large libraries](./research-library-splitting.md) — the
  capability-axis measurements. The locale-axis table in Context comes from
  `bdp-spike/datagen`'s `coll-loc` and `fmt-loc` sets, which reproduce it.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) — how
  the implementation components are consumed and composed.
- [Provider Metadata](./wep-2026-07-26-provider-metadata.md) — why a `core:*` API
  need no longer be CM-representable.
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — the token and
  affine handle kinds the facade draws on.

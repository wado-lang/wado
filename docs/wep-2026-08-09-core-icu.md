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
cleanly, that it tree-shakes by reachability, and that a `no_std` build yields a
zero-import self-contained component. The `bdp-spike/` follow-up established the
other half this design rests on: a capability can take its data from a postcard
blob at run time instead of baking it, with the blob supplied by a component
composed in.

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
implementation asset interns these, so the set is finite by construction and
never needs freeing, and the handle keeps the value semantics the rest of
`core:*` has — it sits in a global, and a closure captures it by copy, so
`list.sort_by(|a, b| c.compare(a, b))` is written the obvious way.

#### Affine resource

A collator tailored from user-supplied rules, a formatter compiled from a runtime
skeleton or message pattern, anything keyed by an `Accept-Language` string, a
lazy segmentation iterator. The far side allocates these per call from input the
program does not bound, so the object has an end and something must reach it: the
handle carries a `dtor` and is move-only.

### One implementation asset, collected per program

Underneath the facade sits a single prebuilt ICU asset, not a partition. It is
shipped relocatable, so the `linking` and `reloc.*` sections survive and the
collection that produced it can run again against the exports one program
reaches (Known gaps). Measured, that reproduces what rebuilding the asset with a
narrower WIT surface would produce, and reaches granularity a rebuild cannot: a
grapheme pass is 23 KB where segmentation's four operations are 2.31 MB.

So the capability axis is reachability's, end to end: code, and the data that
does not vary by locale, which is baked into the asset and collected away when
unreached. What no collection separates is locales, which a constructor reaches
through the same symbol whatever the program declares. That axis alone is the
package's.

### Reachability granularity

The facade carries the coarse split in its types. Where two operations differ in
data by orders of magnitude — a grapheme pass needs kilobytes where word
segmentation pulls a multi-megabyte dictionary — they belong to different types,
so naming a type is already a data decision and the cheap user never mentions
the expensive one.

That split stays load-bearing, because the collection's root set is not free of
Wado's own liveness: the roots are the asset's exports that surviving Wado code
calls, and the `liveness` pass seeds every method of a live type as a live root
by design. A single `Segmenter` type carrying all four operations would keep all
four exports rooted and collect nothing, whatever the program called. Method
granularity is the refinement that would lift that.

### Locales are a compile-time declaration

```wado
use icu from "core:icu" with { locales: ["ja", "en-US"] };
```

- Omitted means root only — collation's root UCA, no tailorings. Minimal by
  default.
- The declared set is the union across all use sites, expanded with
  likely-subtags and the fallback chain.
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
- The implementation asset — the spike's ICU component, promoted to a
  first-party artifact and shipped relocatable. Its locale-independent data is
  baked in; its locale-bearing capabilities take a buffer provider.
- Data assets — the image below, keyed by (capability, locale).

Assembling a program's data is a lookup: the reached locale-bearing capabilities
name their marker sets, the declared locales expand through likely-subtags and
the fallback chain, and the entries at those keys are handed over as blobs.
ICU4X's fork provider lets the asset read several blobs, so nothing merges or
re-encodes them.

Marker granularity finer than the capability is deliberately not pursued. For a
program declaring `ja` and formatting dates, the locale declaration is worth
4.3 MB where choosing within datetime's 57 markers is worth tens of KB inside
the 47 KB that remains. Fixing the marker set per capability is what removes the
ICU-version-specific symbol-to-marker map, and with it the recording test that
would have had to catch the map drifting on every ICU upgrade.

### The data image

One compressed, indexed archive of the locale-bearing data, which keeps `wado` a
self-contained binary while letting a build extract only the entries it needs.

- An entry is exactly what one capability loads for one locale, so a build
  concatenates entries instead of slicing inside them. The index stays small
  because capabilities are tens, where a per-(marker, locale) key would run to
  tens of thousands.
- The index maps entry name to offset and length. Random access is required for
  selective extraction, so a streamed archive format does not qualify.
- A locale's entry is stored deduplicated against its fallback parent, which the
  runtime fallbacker undoes. Measured, that is 5.16 MB down to 4.34 MB on
  formatting and 23 bytes on collation — CLDR patterns inherit heavily where
  collation tailorings are genuinely distinct.
- Compression is per-entry zlib, reversed host-side, so nothing the program
  carries links a decompressor. zlib is chosen over zstd deliberately: it is
  already a compiler dependency and pure Rust, where zstd would add a C
  dependency to an otherwise runtime-free compiler. Measured on the ICU blobs,
  zstd-19's best case is ~24% on mid-size locale data and less elsewhere — not
  enough to buy that. Compression happens once at toolchain-build so its speed
  is irrelevant, and decompression is already negligible against a build that
  pulls a handful of entries.

Collation's runtime dependency on the normalizer's NFD markers — ~37 KB, the one
real cross-capability data dependency measured — needs nothing here: NFD does
not vary by locale, so it is baked into the asset and collation reads it there.

## Alternatives considered

### A resource-free surface keyed by (locale, options)

Every ICU object could be a pure function of a key, with the asset memoizing
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
made a naming decision out of what reachability already decides, so the split
bought nothing — while costing users a memorized map from capability to module,
spreading the locale declaration across three `use` sites to be unioned, and
freezing an internal boundary into the public API where moving it becomes a
breaking change.

### Baking all of the data

ICU4X's compiled-data mode throughout, which the original spike used. Simpler,
and needs no data assets at all, but it cannot slice by locale — a program
declaring two locales still carries every locale CLDR ships. Survivable for
casing and normalization; for formatting it is 4.34 MB where the declared
locales need 47 KB. So baking stays, for the data that does not vary by locale.

### Splitting the asset into components

Several prebuilt components, one per capability, so an unused one is never
composed. Rejected: it coarsens what the collection already does. A partition
is guessed when the artifacts are built and then freezes each capability's
granularity at that guess, where the collection's root set is whatever the
program reached; it also costs a ~10 KB floor per line drawn.

It would additionally need a shared data component to recover what one asset
shares for free — measured, that sharing is ~37 KB between collator and
normalizer and essentially nothing between casemap, properties and segmenter,
dedup following genuine runtime data dependencies rather than taxonomy.

## Consequences

- Users learn one module, and there is no partition to learn or to maintain: one
  asset serves every program.
- Per-program data is bounded by what the program reached and the locales it
  declared; an unreached capability contributes nothing, code and data alike.
- The API tells the truth about cost. A constructed object is visibly
  constructed, and the one-shot helpers beside it are visibly one-shot.
- Toolchain size grows by the bundled ICU package, in exchange for a
  zero-dependency `core:` experience.
- Only locale-bearing data is runtime-loaded, and it pays for that in
  zero-copy-from-static and a deserialization cost per capability. Baking is
  otherwise preferred, being what the collection can reach.
- No ICU code runs at build time, so the build has nothing to sandbox, cache by
  content, or keep deterministic beyond reading its own archive.
- The CM import blocker splits. The compile-time-bounded surface needs only a
  `dtor`-less imported handle; the tailored, runtime-configured, and stateful
  surface waits on full resource import, a gap shared with every
  resource-bearing third-party component.

## Known gaps

- **The prune keeps baked data.** Closed for the capability axis by
  `wado-wasm-embed`'s data-reference pruning, piloted on `wado-bundled-libm`.

  The prune used to drop every function, global, table, tag, type and segment
  unreachable from the exports it kept, but mark every active data segment live
  unconditionally: an active segment initialises memory whether or not anything
  still reads what it wrote. It now prunes an active segment by the byte, from a
  map of which data ranges each function reads, and emits each surviving run as
  a segment of its own. Measured on libm, a program calling `sin` keeps 344 of
  the asset's 5,448 rodata bytes, and every one of the asset's 54 exports
  answers bit-identically to the unpruned module.

  The map comes from `linking` and `reloc.CODE`, which is what wasm-ld collects
  over — `--gc-sections` is its default. Where the asset is consumed as a
  relocatable binary, as ICU's will be, those sections can be read at compile
  time. libm cannot: it is checked in as `.wat`, and the round trip re-encodes
  every relocatable immediate to its narrow form, which moves every byte offset
  a relocation holds. So `mise run update-bundled` resolves the graph once, at
  asset-build time, into a form that names no offset into code — a function name
  and the data ranges it reaches — and the asset carries it in a `wado.dataref`
  custom section. `dataref::resolve` is the shared resolver; only where it runs
  differs.

  Two things follow. Slicing needs no rebuild, so one asset can serve every
  program; and the root set can be finer than any WIT surface a rebuild could
  express. Measured on the ICU asset kept relocatable, collecting against one
  program's exports reproduces the from-source rebuild across every capability —
  89 KB for locale + casemap against a ~92 KB rebuild — and the slices are
  runtime-checked, Turkish casing included, so the graph carries the data edges
  and not only the code.

  This closes the capability axis only. A locale is reached through the same
  symbol whatever the program declares, so no collection separates `ja` from the
  rest — that stays the data image's job.

  What the asset still needs is in Implementation: the collection has to reach
  inside a component, resolve the map at compile time from the sections a
  relocatable asset keeps, and carry the `reloc.DATA` edges libm does not have.

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
- [ ] The full capability inventory, and which of its data is locale-bearing.
      The first cut covers what the spikes measured — locale identity,
      normalization, case mapping, character properties, segmentation,
      collation — with formatting, plural rules, and transliteration following.

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
- [ ] Promote the spike's ICU component into a first-party prebuilt artifact,
      shipped relocatable, with the locale-bearing capabilities moved onto a
      buffer provider and the rest left baked.
- [ ] Collect over a **component** asset. `wado-wasm-embed` prunes a core-module
      asset; a component takes the `wasm-compose` path instead and is composed
      whole, so nothing prunes ICU's asset today — neither its code nor its
      data. The root set is the same one the core path already uses: the
      asset's exports that surviving Wado code calls.
- [ ] Resolve the data-reference map at compile time. `dataref::resolve` reads
      `linking` and `reloc.CODE` and is run today only by
      `mise run update-bundled`, because libm is checked in as `.wat` and the
      round trip invalidates the offsets a relocation holds. An asset shipped
      relocatable keeps those sections, so `embed` can resolve from them
      directly and needs no baked `wado.dataref`.
- [ ] Carry `reloc.DATA` — a pointer stored in the data itself, reaching another
      data range or a function. The map's shape and the walk both have to grow:
      a live data range can root a function, so the data and code edges close
      over one worklist rather than the code seeding the data once. The resolver
      rejects one today rather than resolving halfway.
- [ ] Write the facade over it: re-exports, the Wado-native shapes (`Ord`,
      iterators, `Display`), and the one-shot helpers.
- [ ] Build the data image — per-(capability, locale) zlib entries plus index,
      deduplicated against the fallback parent — and bundle it with the
      toolchain.
- [ ] Feed several blobs to the asset through ICU4X's fork provider.

## References

- [Compile-Time Data Providers](./wep-2026-06-13-compile-time-data-providers.md)
  — the mechanism this consumes, here reduced to an archive lookup.
- [research: splitting large libraries](./research-library-splitting.md) — the
  capability-axis measurements. The locale-axis table in Context comes from
  `bdp-spike/datagen`'s `coll-loc` and `fmt-loc` sets, which reproduce it.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md) — how
  the implementation asset is consumed and composed.
- [Provider Metadata](./wep-2026-07-26-provider-metadata.md) — why a `core:*` API
  need no longer be CM-representable.
- [Resource Ownership](./wep-2026-05-21-resource-ownership.md) — the token and
  affine handle kinds the facade draws on.

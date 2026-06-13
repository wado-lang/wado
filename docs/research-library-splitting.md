# Research: Splitting Large Libraries into Components

How to break a large library (think ICU4X, but the reasoning is general) into
Wasm Component Model (CM) pieces so a program pays only for what it uses. This
is a findings document grounded in two measured spikes under
`wado-bundled-icu/`:

- `wado-bundled-icu/` — the whole library baked into one self-contained
  component (the monolith baseline).
- `wado-bundled-icu/bdp-spike/` — data-free feature components that load data at
  runtime, plus a shared data component (reproduce with `bdp-spike/build.sh`).

It deliberately stops short of prescribing a concrete partition for ICU; that is
a separate design discussion.

## The two things you split: code and data

A large library's footprint has two very different parts:

- **Code** — the algorithms. Usually the smaller part, and Rust LTO already
  tree-shakes it by reachability, so the *public surface* (the WIT exports you
  keep) is the size knob.
- **Data** — baked tables (Unicode/CLDR, lookup tries, dictionaries). Often the
  dominant part, and it does **not** tree-shake the way code does: it is pulled
  in by whichever code paths survive.

Splitting strategy differs per part. Code is shrunk by *narrowing the surface*;
data is shrunk by *not baking what you don't use* and *not storing what you use
twice*.

## What the Component Model lets you share

CM components have **separate linear memories**. The only things that cross a
component boundary are Canonical-ABI values — `string`, `list`, `record`,
`enum`, `result`, and `resource` handles. Consequences:

- You **cannot** share in-memory state or data structures across components.
  Anything a component computes over must live in *its own* memory.
- You therefore cannot make library A "use" library B's internal data by merely
  importing B's *interface* — unless A actually calls B across the boundary for
  each operation, which is a non-starter for hot, per-element data access.
- What you *can* share cheaply is **bytes**: a component can hand another
  component a `list<u8>` (e.g. a serialized data blob), which the receiver
  deserializes into its own memory.

This last point is the lever that makes data sharing possible despite separate
memories.

## Three levers

1. **Split by feature (surface narrowing).** Put independent capabilities behind
   separate WIT exports / components. LTO then keeps only the code each one
   reaches. A program embeds only the feature components it uses, so unused
   features — and their data — never ship.

2. **Separate data from code (runtime data provider).** Build feature components
   *without* baked data and have them load a serialized data blob at runtime
   through a provider abstraction. The blob is produced offline by a datagen
   step that includes only the markers the chosen features need. This turns data
   into a separate, sliceable, swappable artifact and makes feature components
   tiny.

3. **Compose a shared data component.** Bake the blob into one component that
   exports it, and have feature components `import` it. Composition wires them
   into a single self-contained artifact, so "the shared part lives in its own
   component, the others use it" is realized literally — for *data*, the thing
   that actually crosses the boundary as bytes.

Levers 2 and 3 only help where data is the cost. Lever 1 is always the first
move, because the biggest data is usually feature-specific and simply absent
when you don't embed that feature.

## ICU4X: what we measured

ICU4X is `compiled_data` by default (every component bakes its CLDR/Unicode
data). It also exposes `*_with_buffer_provider` constructors + `BlobDataProvider`
+ an offline `datagen`, which is exactly the lever-2/3 machinery. The spike
exercised all three levers; the load-bearing results:

- **Data-free feature components work.** A casemap feature built with
  `default-features = false` (no baked data), loading a postcard blob at runtime,
  produced correct locale-aware Unicode (Turkish `İ`, Greek, `ß`→`ss`). It is
  **45 KB** with zero data; the sliced casemap blob is **23 KB**.
- **The blob can come from a shared component.** A data component baking the blob
  and exporting it was composed with the feature component into one import-free
  component (**78 KB**) — no host involvement.
- **Cross-feature dedup is real, but only for a genuine runtime data
  dependency.** The collator's constructor *requires* the normalizer's NFD
  markers, so a collator feature and a normalizer feature loading one shared blob
  store those markers once: measured dedup **37 KB** (and the collator correctly
  treats decomposed vs precomposed text as canonically equal, proving it reads
  the shared data).
- **"Conceptually related" is not enough.** casemap + properties + segmenter were
  expected to share `icu_properties` data; measured dedup was **3 bytes**. ICU4X
  bakes derived property data into each consumer's *own* markers
  (`SegmenterBreak*`, `CaseMap*`, `PropertyEnum*`), so the marker sets are
  disjoint. Segmenter alone is **4.15 MB** (LSTM + dictionary) — feature-specific
  data that no amount of sharing removes.

## Principles

- **Slice first, dedup second.** Embedding only the features you use removes the
  dominant (feature-specific) data. Dedup is a bounded bonus on top.
- **Dedup follows runtime data dependencies, not taxonomy.** Two features share
  storage only when one's code actually requests the other's data markers. Check
  the constructor's data bounds, not the domain.
- **Make data an artifact.** A provider + offline datagen turns data into a
  sliceable, shareable, versionable blob and shrinks feature code to algorithms.
- **A shared component dedups stored bytes, not working sets.** Each consumer
  still deserializes into its own memory (CM has no shared memory); the win is in
  the shipped blob, not runtime RAM.
- **Mind the boundary cost.** Runtime-loaded data loses the zero-copy-from-static
  property of baked data, and the provider/deserialization code adds a fixed
  per-feature overhead — so for a *single* feature, separation is roughly size-
  neutral. It pays off across multiple features and via slicing.

## Open questions (deferred)

- The concrete partition for ICU on Wado: which capabilities become components,
  how coarse, and where the import edges go. (Left for design discussion.)
- How datagen plugs into the Wado build pipeline (when/where blobs are produced
  and sliced for a given program).
- Whether to expose one shared data component or per-feature data, and how that
  interacts with marker-level dedup between dependent features.

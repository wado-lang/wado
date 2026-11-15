# `core:icu` TODO

Open work on `core:icu` and the ICU asset behind it. [`AGENTS.md`](./AGENTS.md)
says how to work here, and
[WEP: `core:icu`](../docs/wep-2026-08-09-core-icu.md) holds the design these
items finish.

The roadmap is ordered, and each item says what finishing it means. A known gap
says what is missing and what it admits, not how to close it. A finished item
leaves this file, and the documents here state what it built.

## Roadmap

1. [ ] The [data-provider mechanism](../docs/wep-2026-06-13-compile-time-data-providers.md)
   itself, reduced here to an archive lookup: `core:icu` runs no provider
   component and needs no compile-time execution.
2. [ ] Ship the asset relocatable, and move the locale-bearing capabilities onto
   a buffer provider with the rest left baked. The asset is first-party and
   bundled already; what it is not yet is either of those.
3. [ ] Grow the facade past character properties: re-exports, the Wado-native
   shapes (`Ord`, iterators, `Display`), and the one-shot helpers.
4. [ ] Build the data image — per-(capability, locale) zlib entries plus index,
   deduplicated against the fallback parent — and bundle it with the
   toolchain.
5. [ ] Feed several blobs to the asset through ICU4X's fork provider.

## Known gaps

- The collection reaches an interface's lifts, not a world's. A world-level
  lift is a root whether or not the program calls it, because the import plan
  lists every world function the dependency declares and `used_wasi_functions`
  keys effect operations alone
  ([#2068](https://github.com/wado-lang/wado/issues/2068)). `core:icu` exports
  interfaces only, so it costs nothing here.
- The collection stops at the export. A program reaching one binary property
  carries the code and data of all 63. Their exports take the position of a
  case and match over every one, so naming `Uppercase` does not narrow what the
  function roots. An enumerated property does not have this gap: its values
  share one map, which any of them reaches anyway.
- A dropped lift leaves a stub, not a hole. A lift the program does not import
  keeps its name and signature in the core module, because the component's
  `canon lift` still aliases it. Dropping it outright would mean renumbering the
  component's own index spaces. The data and code a lift reached are gone, but
  its stub, its export name and the component's alias of it remain. With three
  exports per property that comes to kilobytes in every program that uses
  `core:icu` at all.
- A collator takes no options. The keywords a `-u-` extension carries (`co`,
  `kn`, …) are dropped with the extension, and `Collator::new` takes nothing
  else, so a program cannot ask for phonebook order or numeric collation.
- No Turkic simple folding. The `T` entries of `CaseFolding.txt` map `I` to `ı`
  and `İ` to `i` for Turkish and Azerbaijani. ICU4X offers them as
  `simple_fold_turkic`, and `Locale::fold` already applies them to strings.
  Nothing reaches them for one character or for a set.
- No inverse for the three simple case mappings. A caller that wants every
  character whose `simple_lowercase` lands in a set has no set operation for
  it, only `simple_fold_closure`.
- Gale's `caseInsensitive` widens a range by ASCII case alone, and whether
  ANTLR4 widens it further is unsettled.

## Open questions

- [ ] A closure cannot own an affine value
  ([Affine Types](../docs/wep-2026-10-17-affine-types.md#a-closure-cannot-own-an-affine-value)),
  so no function returns a comparator that carries its collator, and a map
  keyed by `Accept-Language` cannot hold collators either
  ([maps and sets of affine values](../docs/wep-2026-10-17-affine-types.md#maps-and-sets-of-affine-values)).
  Nor can a global hold one ([Affine Types](../docs/spec-affine.md)), so a
  collator built once per program is passed down by `&`.
- [ ] Does ICU4X expose collation sort keys? A bulk key-extraction call would
  let a sort cross the boundary once instead of per comparison, which
  dominates any handle-versus-lookup difference.
- [ ] Where the facade's date/time formatting meets
  [`core:temporal`](../docs/wep-2026-06-05-core-temporal.md), which takes its
  zone IANA canonicalization from here — WASI's `timezone` answers only for the
  host's own zone, so it cannot serve one, and ICU4X's own zone data stops at
  identifiers: `TimezonePeriodsV1` is 6.8 KB for every zone and holds a
  standard/daylight pair, not the transitions that say which is in effect.
  Open: whether the tzdb the offsets do need rides this facade's data component
  as one more marker set, or arrives beside it.
- [ ] The full capability inventory, and which of its data is locale-bearing.
  The first cut covers what the spikes measured — locale identity,
  normalization, case mapping, character properties, segmentation, collation —
  with formatting, plural rules, and transliteration following.

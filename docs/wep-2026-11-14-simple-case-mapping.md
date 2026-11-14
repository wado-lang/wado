# WEP: Simple Case Mapping in `core:icu`

## Context

[Re](./wep-2026-11-02-re.md) refused a pattern that turns on the `i` modifier.
ECMA-262 defines `i` through `Canonicalize` (§22.2.2.7.3): in Unicode mode, two
characters match when their simple case foldings are equal. Simple case folding
is `scf`, the `C` and `S` entries of `CaseFolding.txt`. It maps one character
to one character.

`core:icu` offers only the full mappings, over whole strings: `Locale::fold`,
`uppercase` and `lowercase`. Full folding is the wrong relation for matching one
character at a time, in both directions:

- It maps one character to several. `ß` full-folds to `ss`, but under `scf` it
  stays `ß` and only `ẞ` (U+1E9E) matches it.
- It merges what `scf` keeps apart. `İ` (U+0130) full-folds to `i̇`, but its
  `scf` is itself, so under ECMA-262 `/İ/i` matches `İ` alone.

So Re cannot build `i` from what `core:icu` has. Re needs `Simple_Case_Folding`
alone, but it is one of four simple mappings the UCD defines as character
properties, with `Simple_Lowercase_Mapping`, `Simple_Uppercase_Mapping` and
`Simple_Titlecase_Mapping`. No package uses the other three today.

Matching asks a second question that no per-character mapping answers cheaply.
Re compiles each character class to the UTF-8 byte sequences of its code
points, so under `i` it needs every character that folds to the same value as
some member of the class. That is the inverse of `scf`, and finding it from
`scf` alone means asking all 1,114,112 code points.

## Decision

### Four functions for the four simple mappings

```wado
pub fn simple_lowercase(ch: char) -> char;
pub fn simple_uppercase(ch: char) -> char;
pub fn simple_titlecase(ch: char) -> char;
pub fn simple_fold(ch: char) -> char;
```

Each answers the UCD property of the same name. A character the property does
not map answers itself. The three that Re does not use ship with
`simple_fold`, so that `core:icu` answers the simple mappings as a family
rather than the one a first consumer needed.

They are free functions, beside `contains` and `ranges`. The
[`core:icu` WEP](./wep-2026-08-09-core-icu.md#two-api-shapes-chosen-by-what-the-thing-is)
puts an operation on `Locale` when a locale tailors it. These four are
properties of the character, and Unicode defines them without a locale.
`simple_fold` is `scf` as `CaseFolding.txt` defines it, the `C` and `S` entries,
which is what ECMA-262 names. The `T` entries, the Turkic tailoring, are not
part of it; see Known gaps.

Each crosses the boundary as one `u32` and answers one, through ICU4X's
`CaseMapper::simple_*`. Each has its own export, so a program that calls only
`simple_fold` keeps only what `simple_fold` reaches. The data is the case
mapping data that `Locale::fold` already reaches.

### One function for the closure of a set under simple folding

```wado
pub fn simple_fold_closure(ranges: List<[u32, u32]>) -> List<[u32, u32]>;
```

It answers every code point whose `scf` equals the `scf` of some code point in
`ranges`. The answer contains `ranges`. Its ranges are inclusive, disjoint,
lowest first, and no two touch, the shape `ranges` already answers. The input
may be in any order, overlap, or touch.

The set operation is the second half of the API because the inverse of `scf`
is the expensive part, and it costs the same for every caller. It runs inside
the asset. Done in Wado from `simple_fold`, it would ask every code point, each
a boundary crossing, as Context says.

The function takes a list of ranges rather than a `CodePointSet`. Its typical
input is a set the caller built, such as a regular expression's character
class, and not a property. A property passes through `ranges(p)` first.

The closure is built from the definition of `scf`. `wado-bundled-icu/gen`
applies `CaseMapper::simple_fold` to every code point and writes the classes
of more than one member, each a list of the code points that fold to one
value. `simple_fold_closure` adds every class with a member in the input. The
table is generated with the WIT, the Rust tables and the Wado enums, so it
follows the ICU4X the asset is built from.

| ICU4X | Classes | Code points in them | Of 2 members | Of 3 | Of 4 |
| ----- | ------: | ------------------: | -----------: | ---: | ---: |
| 2.3.0 |    1482 |                2994 |         1455 |   24 |    3 |

A Rust program over `icu_casemap` 2.3.0 that groups every code point by
`CaseMapper::simple_fold` gives these counts.

ICU4X's own closure, `add_case_closure_to`, is not the source. It is
documented over a different relation, full case folding together with the
simple mappings, and adds strings for it (`ss` for `ß`, `i̇` for `İ`). Its
characters alone equal the `scf` classes at ICU4X 2.3.0, but only as an
observation that no version promises to keep.

### What Re builds on it

Re's parser stops refusing `i`. Under `i`, each atom compiles as follows:

- A character becomes the closure of the set holding it alone.
- A class becomes the closure of its set. A negated class `[^…]` complements
  the closure, as `CharacterSetMatcher` (§22.2.2.7.1) inverts the result of the
  canonicalized membership test.
- `\w`, `\b` and `\B` use the closure of `[A-Za-z0-9_]`, which adds U+017F and
  U+212A. `WordCharacters` (§22.2.2.9.3) states the set differently, as the
  characters whose `scf` lies in `[A-Za-z0-9_]`. The two are equal because
  `scf` maps `[A-Za-z0-9_]` into itself.

The closure runs once, when the pattern compiles, so matching decodes and folds
nothing. Re rejects backreferences, so it never compares two captured strings
caselessly at match time.

## Roadmap

- [x] `simple_lowercase`, `simple_uppercase`, `simple_titlecase` and
  `simple_fold` in the asset and the facade. The test checks that each
  function reaches its own mapping, on a character where it differs from the
  others; ICU4X's answers themselves are not retested.
- [x] `simple_fold_closure`, over the table `gen` writes. Its tests cover the
  closure's own logic: a class joins whole when one member is in the input,
  the input survives, and unsorted, overlapping or touching input comes back
  in the documented shape. A program that already maps case pays nothing for
  either item:

  | The program asks                        | Size     |
  | --------------------------------------- | -------- |
  | `println("a")`, no `core:icu`           | 4,501 B  |
  | `simple_fold('A')`                      | 57,559 B |
  | `simple_fold_closure([[107, 107]])`     | 73,459 B |
  | `Locale::uppercase`, on `main` and here | 85,329 B |

  Each is `wado compile -O2` of a `run` that asks it once and prints the
  answer, all with one `wado` binary.
- [x] Re accepts `i`. The differential test against Node's `RegExp` with the `u`
  flag covers it, and so do test262's case-insensitive RegExp tests. The
  known gap in [Re](./wep-2026-11-02-re.md#known-gaps) is removed.

## Known gaps

- No Turkic simple folding. The `T` entries of `CaseFolding.txt` map `I` to
  `ı` and `İ` to `i` for Turkish and Azerbaijani. ICU4X offers them as
  `simple_fold_turkic`, and `Locale::fold` already applies them to strings.
  Nothing here reaches them for one character or for a set.
- No inverse for the three simple case mappings. A caller that wants every
  character whose `simple_lowercase` lands in a set has no set operation for
  it, only `simple_fold_closure`.
- Gale's `caseInsensitive` widens a range by ASCII case alone, and whether
  ANTLR4 widens it further is unsettled.

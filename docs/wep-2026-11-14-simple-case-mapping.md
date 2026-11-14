# WEP: Simple Case Mapping in `core:icu`

## Context

[Re](./wep-2026-11-02-re.md) refuses a pattern that turns on the `i` modifier.
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

So Re cannot build `i` from what `core:icu` has. The gap is not Re's alone. The
simple mappings are character properties of the UCD (`Simple_Lowercase_Mapping`,
`Simple_Uppercase_Mapping`, `Simple_Titlecase_Mapping`, `Simple_Case_Folding`).
A lexer, a case-insensitive identifier table, or anything else that compares
text one character at a time needs them, and only ICU knows them.

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
not map answers itself.

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
the asset, over ICU4X's case data. Done in Wado from `simple_fold`, it would
take one boundary crossing per code point of the input class. A class such as
`\p{L}` holds more than 100,000.

The function takes a list of ranges rather than a `CodePointSet`. Its typical
input is a set the caller built, such as a regular expression's character
class, and not a property. A property passes through `ranges(p)` first.

Inside the asset, the closure works over the orbits of `scf`: each orbit is a
fold target together with every character that folds to it. An orbit with a
member in the input joins the answer whole. The orbits come from the
characters that `Changes_When_Casefolded` lists, which include every character
whose `scf` differs from itself. They are computed on the first call rather than
generated into a table, so no second copy of ICU's case data must agree with
the first after an ICU upgrade.

ICU4X's own closure, `CaseMapCloser::add_case_closure_to`, is not used. It
closes over full case folding and the simple case mappings, so it adds strings
(`ss` for `ß`) and joins `i` to `İ`. That is the relation `Locale::fold`
already answers for strings, not the one character matching needs.

### What Re builds on it

Re's parser stops refusing `i`. Under `i`, each atom compiles as follows:

- A character becomes the closure of the set holding it alone.
- A class becomes the closure of its set. A negated class `[^…]` complements
  the closure, as `CharacterSetMatcher` (§22.2.2.7) inverts the result of the
  canonicalized membership test.
- `\w` becomes the closure of `[A-Za-z0-9_]`, which adds U+017F and U+212A, as
  `WordCharacters` (§22.2.2.9.4) says. `\b` and `\B` test the same set.

The closure runs once, when the pattern compiles, so matching decodes and folds
nothing. Re rejects backreferences, so it never compares two captured strings
caselessly at match time.

## Roadmap

- [ ] `simple_lowercase`, `simple_uppercase`, `simple_titlecase` and
  `simple_fold` in the asset and the facade. A test holds `simple_fold` to
  the cases in Context, `ß`, `ẞ` and `İ`, and to the Kelvin sign folding to
  `k`.
- [ ] `simple_fold_closure`. A Rust test in `wado-bundled-icu` checks it against
  `CaseMapper::simple_fold` over every code point.
- [ ] Re accepts `i`. The differential test against Node's `RegExp` with the `u`
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

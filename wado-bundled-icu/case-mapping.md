# Simple Case Mapping

`core:icu` maps case two ways. `Locale::uppercase`, `lowercase` and `fold` map
whole text in full, and a locale tailors them. The simple mappings map one
character to one, and no locale tailors them. This document is about the
second kind.

## Why full mapping is not enough

ECMA-262 defines a regular expression's `i` through `Canonicalize`
(§22.2.2.7.3): in Unicode mode, two characters match when their simple case
foldings are equal. Simple case folding is `scf`, the `C` and `S` entries of
`CaseFolding.txt`. Full folding is the wrong relation for matching one
character at a time, in both directions:

- It maps one character to several. `ß` full-folds to `ss`, but under `scf` it
  stays `ß` and only `ẞ` (U+1E9E) matches it.
- It merges what `scf` keeps apart. `İ` (U+0130) full-folds to `i̇`, but its
  `scf` is itself, so under ECMA-262 `/İ/iu` matches `İ` alone.

Matching asks a second question that no per-character mapping answers cheaply.
Re compiles a character class to the UTF-8 byte sequences of its code points,
so under `i` it needs every character that folds to the same value as some
member of the class. That is the inverse of `scf`, and finding it from `scf`
alone means asking all 1,114,112 code points.

## Four functions for the four simple mappings

`simple_lowercase`, `simple_uppercase`, `simple_titlecase` and `simple_fold`
each answer the UCD property of the same name (`Simple_Lowercase_Mapping` and
so on), and a character the property does not map answers itself. Re uses
`simple_fold` alone; the other three ship with it so that `core:icu` answers
the simple mappings as a family.

They are free functions, beside `contains` and `ranges`. The WEP
[puts an operation on `Locale`](../docs/wep-2026-08-09-core-icu.md#two-api-shapes-chosen-by-what-the-thing-is)
when a locale tailors it, and Unicode defines these four without one.
`simple_fold` is `scf` as `CaseFolding.txt` defines it, which is what ECMA-262
names; the Turkic `T` entries are not part of it.

Each crosses the boundary as one `char` through ICU4X's `CaseMapper::simple_*`,
and has its own export, so a program keeps only the ones it calls. The data is
the case mapping data `Locale::fold` already reaches.

## The closure of a set under simple folding

`simple_fold_closure(ranges)` answers every code point whose `scf` equals the
`scf` of some code point in `ranges`. Both lists hold inclusive ranges; the
answer comes lowest first, with no two touching, and the input may come in any
order, overlap or touch.

It is the expensive half of the API, so it runs inside the asset: done in Wado
from `simple_fold`, it would cross the boundary once per code point. It takes
ranges rather than a `CodePointSet` because its typical input is a set the
caller built, such as a character class.

The closure is built from the definition of `scf`. `gen/` applies
`CaseMapper::simple_fold` to every code point and groups those that fold to one
value into classes. It writes the members of the classes of more than one into
`src/casemap/fold_classes.rs`, sorted by code point, with each member linked to
the next one in its class so that following the links from any member visits
its class. The closure searches the members for each range of the input and
adds the classes of those it finds, over ICU4X's inversion lists, so its cost
follows the input rather than the table. The table is regenerated with the rest
of `gen/`'s outputs, so it follows the ICU4X the asset is built from.

| ICU4X | Classes | Code points in them | Of 2 members | Of 3 | Of 4 |
| ----- | ------: | ------------------: | -----------: | ---: | ---: |
| 2.3.0 |    1482 |                2994 |         1455 |   24 |    3 |

Grouping `MEMBERS` in `src/casemap/fold_classes.rs` by its `NEXT` cycles gives
these counts.

ICU4X's own closure, `add_case_closure_to`, is not the source. It is
documented over a different relation, full case folding together with the
simple mappings, and adds strings for it (`ss` for `ß`, `i̇` for `İ`). Its
characters alone equal the `scf` classes at ICU4X 2.3.0, but only as an
observation that no version promises to keep.

## What a program carries

| The program asks                    | Size     |
| ----------------------------------- | -------- |
| `println("a")`, no `core:icu`       | 4,501 B  |
| `simple_fold('A')`                  | 57,559 B |
| `simple_fold_closure([[107, 107]])` | 67,550 B |
| `Locale::uppercase`                 | 85,329 B |

Each is `wado compile -O2` of a `run` that asks it once and prints the answer,
all with one `wado` binary. `Locale::uppercase` measured the same before the
simple mappings were added.

Re closes every set it compiles under `i`, and any pattern read at run time
may turn `i` on, so a program that uses Re carries the closure whether or not
its patterns do. `Regex::new("a+b").unwrap().is_match("aab")` compiles to
481,823 B without `i` support and 506,397 B with it.

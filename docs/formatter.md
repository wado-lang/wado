# Wado Formatter

`wado format` re-prints a source file in canonical form. This document is the
rules it follows, and how those rules are held in place by tests.

## One style, by design

The formatter has no configuration. There is no line-width setting, no brace or
indent option, no directive that turns it off for a region, and no plan to add
one. The only input besides the source is the `[format] exclude` list in
`wado.toml`, which decides which files are formatted at all, never how.

A single style is worth more than any option it forgoes. Every Wado file reads
the same way, a diff shows what changed rather than who wrote it, layout never
reaches code review, and one canonical output is a thing tests can pin exactly.

## Contract

Four promises, each covered by a test in [Tests](#tests).

- The output re-parses to the same program. Only layout changes.
- The output is a fixed point: formatting it again returns it unchanged.
- Every comment survives. A construct may move one, never drop it.
- A file that does not parse is not formatted, and the error names the file,
  line, and column.

## Running it

`wado format FILE` prints the result. `-w` rewrites files in place and
`--check` exits non-zero when a file would change; more than one input requires
one of the two. `mise run format` formats the whole repository, Rust and
Markdown included.

`[format] exclude` in a `wado.toml` skips paths. `wado-compiler` excludes
`tests/**`, because those fixtures have hand-authored layouts that are part of
the test. The exclusion applies to the directory walk, not to a path you name,
so never run `wado format -w` on a fixture path.

## Layout

A line is at most 120 columns.

A construct is rendered compact and wrapped only when it does not fit. The
budget counts what will follow on the same line, such as the `{` after an `if`
condition, so a construct never fits by ignoring what comes next.

Four things force the wrapped form whatever the width allows.

- A trailing comma in the source. Writing `S { x: 1, }` asks for one field per
  line, and the request round-trips.
- A nested container. A struct literal or array holding another one breaks, and
  each nested container takes a line of its own.
- More than one element bearing a call, which keeps a dense line readable.
- A comment inside the construct that the compact form has no place for.

A flat array that does not fit packs as many elements per line as the budget
allows. Every other wrapped list is one entry per line with a trailing comma.
Declaration bodies, a `match` with more than one arm, and `if` / `else` chains
are always multi-line.

Blank lines follow the source: none stays none, one stays one, and a larger gap
collapses to two.

## What it rewrites

Only what carries no meaning: redundant parentheses, an explicit `-> ()` return
type, repeated or missing statement semicolons, an `impl` block's type
parameters (always written out), and a field name's quoting (bare when it is an
identifier).

Everything else is kept as written, including the spelling of every literal
(hex, octal, digit separators, float form, string and template escapes),
`&self` receivers, compound assignments, comparison chains, struct shorthand,
visibility keywords, and any parenthesis that changes how the code parses.

## Comments

A comment can sit in any gap between two tokens, and most of those gaps belong
to no node of the syntax tree. The rules below hold for all of them.

- A comment is never dropped.
- Where the construct around it has a place for one, it stays there: between
  two list entries, before a closing delimiter, or at the end of an entry's
  line.
- A construct with no such place moves the comment outward, to the next
  statement, item, member, or closing brace. It never leaves the declaration it
  was written in unless nothing inside that declaration can hold it.
- A comment only ever moves outward. It is never adopted by a construct it was
  not written inside, and never lands between a doc comment and what that
  comment documents.
- A block comment before a list entry stays on that entry's line, so
  `foo(/*flag=*/true)` reads as the argument's annotation. A line comment takes
  a line of its own.
- A comment inside a construct forces it to wrap, so that there is a line to
  put the comment on.
- A moved comment does not carry the blank lines that surrounded it. The gap
  they described is not the gap it lands in.
- A comment inside a `${…}` interpolation is a comment like any other.

The rules are chosen so the result is a fixed point. A comment is moved to
where the next parse will read it, which is why a second pass agrees with the
first.

As a backstop, `format` compares the comments in its input and its output and
refuses to emit a file that lost one. The rules above make that unreachable.

## Tests

`wado-compiler/tests/format.rs` holds them all.

`test_format_keeps_a_comment_wedged_in_any_token_gap` is the one that makes the
comment rules a guarantee rather than an intention. It wedges a line comment
and a block comment into every gap between two tokens of every fixture,
template interpolations included, and asserts that each variant formats, keeps
the comment, re-parses, and is a fixed point. Comment loss is a class of defect
rather than a list of sites, so it is enumerated mechanically: a construct that
ignores comments is caught here, not by a user whose file stops formatting.

`assert_format_preserves_ast` is the per-case helper behind the first two
promises. It formats the source, compares the parsed trees ignoring positions,
and asserts the fixed point. Reach for it in any test that adds a syntax.

The golden tests pair a `tests/format.fixtures/*_dirty.wado` input with its
expected output under `tests/generated/format.fixtures/`. They are where a
layout rule is pinned exactly, which is what a single style buys. Add a fixture
when a rule is added or changed, then regenerate with
`mise run update-golden-format-fixtures` and read the diff.

`test_format_idempotent_all_fixtures` and `test_no_dropped_comments_in_corpus`
run the whole fixture and standard-library corpus through the formatter, so a
rule that only misbehaves at scale still has somewhere to fail.

# Overview of Docs

This is the documentation directory of Wado. The `markdown` skill holds the
rules for every Markdown file; these add what is particular to `docs/`.

- Implementation details belong only in a WEP; anywhere else they go stale
  unseen.
- A document's first `#` heading is its title in the index below. After adding,
  removing, or retitling one, run `mise run update-docs-index`.

## Specification

`docs/spec-*.md` is normative (`spec-overview.md` says what that means), one
file per area, each rule stated once. It says what a rule is; how it came to be
is the WEP's.

It states how the language should behave. A shortfall in the compiler is a known
gap in the WEP that proposed the rule, except a rule adopted but not yet built,
which carries a `> Not yet implemented: …` note.

A `wado` code block quotes an e2e fixture or a source file, named in an HTML
comment before it (`<!-- {"fixture": "name.wado"} -->` or
`<!-- {"source": "path/from/root"} -->`); `mise run check-spec-examples` holds it
([WEP](./wep-2026-09-26-spec-examples.md)).

A change that settles a rule writes it into the specification in the same
change. A file stays readable in one sitting. A new file takes its place in the
`## Chapters` list of `spec-overview.md`, which the index follows.

## WEP: Wado Evolution Proposals

A WEP proposes a design, for a user-visible feature or the compiler's
architecture: `docs/wep-YYYY-MM-DD-{feature-name}.md`, with these sections.

- Title
- Context: the problem
- Decision: what was decided and why, including what is deliberately left out
- Roadmap (optional): what will be done, in order, each entry saying what
  finishing it means
- Known gaps (optional): what is missing and what it admits, with no claim that
  it will be closed, and never how to close it

Once a design settles, its rules move to the specification, which wins any
disagreement.

Adopting or refusing a language feature is the human's call. A consequence of
an adopted decision needs no new approval; a choice the decision leaves open is
a known gap. Moving an item between Roadmap and Known gaps is the human's call.
A statement the work shows impossible is corrected in the WEP, not worked
around in the code.

An open question gets no Decision and no Roadmap for its open part. A
comparison with another language is welcome; its useful half is what that
language's users complain about. Work set aside for something that paid more is
deferred, not dead: say what it lost to, and keep the shape of the work.

## Index

@README.md

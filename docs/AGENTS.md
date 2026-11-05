# Overview of Docs

This is the documentation directory of Wado.

## Rules for Markdown

The `markdown` skill holds the rules for every Markdown file. These add what is
particular to `docs/`.

- Don't document implementation details outside a WEP. They go stale, and a reader this far from the code has no way to notice.
- A document's first `#` heading is its title in the index below.
- After adding, removing, or retitling a document, run `mise run update-docs-index`.

## Specification

The specification is `docs/spec-*.md`, one file per area of the language. It is
normative, and `spec-overview.md` says what that means. Each rule is stated in
exactly one place. The specification says what a rule is. How the rule came to
be belongs to the WEP that proposed it.

The specification states exactly how the language should behave. It is not the
place for implementation details or bugs. Where the compiler falls short of a
rule, the shortfall is a known gap in the WEP that proposed the rule. The one
exception is a rule adopted but not built yet: it carries a
`> Not yet implemented: …` note, as `spec-overview.md` § Status says.

A `wado` code block quotes an e2e fixture or a source file, named in an HTML
comment before it (`<!-- {"fixture": "name.wado"} -->`, or
`<!-- {"source": "path/from/root"} -->`). `mise run check-spec-examples` holds
this, and
[WEP: Spec Examples Quote Fixtures](./wep-2026-09-26-spec-examples.md) says
what it asks of the block and the fixture.

A change that settles a rule writes it into the specification in the same
change. A file stays readable in one sitting; an area that outgrows that splits
into two files.

The `## Chapters` list in `spec-overview.md` is the reading order, and the
index follows it. A new spec file takes its place in that list, or
`mise run update-docs-index` fails.

## WEP: Wado Evolution Proposals

A WEP is a proposal: the problem, the design decided for it, and the work to
get there. It covers user-visible features and compiler architecture alike.

Filename: `docs/wep-YYYY-MM-DD-{feature-name}.md`

- Title: Short description of the proposal
- Context: Background and problem statement
- Decision: What was decided and why
- Roadmap (optional): What will be done, in order
- Known gaps (optional): What is missing, whether or not it will be closed

Once a design settles, its rules move to the specification, and the WEP stops
being where a reader looks a rule up. Where a WEP and the specification
disagree, the specification holds.

Adding or changing a language feature is the human's call, to adopt and to
refuse alike. Propose it and wait. Recording a feature that already exists is
not that call, whoever wrote it.

A consequence of an adopted decision needs no new approval. Write it into the
WEP, and name the decision it follows from where that is not obvious. A choice
the decision leaves open is not a consequence: record it as a known gap,
however small, for the human to settle.

A WEP is a policy, not a law. Keep looking for a better way than the one it
states, and propose one when you find it: adopting it is still the human's
call. A statement the work shows to be impossible is the WEP's to change, not
the code's to work around. Correct it to what is true.

Roadmap and Known gaps split on commitment, not on size. A roadmap item will be
done, so it is ordered and each entry says what finishing it means. A known gap
is known and unowned: what is missing and what it admits, with no claim that it
will be closed. Demoting a roadmap item to a gap, or promoting a gap, is the
human's call.

A gap does not say how to close it. Whoever comes to it should think from zero.
A written approach anchors them to what its writer saw before the problem was
understood.

No "out of scope" section: an unfinished mechanism is a known gap. A deliberate
omission goes in Decision.

An open question gets no Decision and no Roadmap for its open part: the WEP
states the problem, not an answer nobody chose. A comparison with another
language is welcome; its useful half is what that language's users complain
about. Work set aside for something that paid more is deferred, not dead: say
what it lost to and what was already done, not how to finish it.

## Index

@README.md

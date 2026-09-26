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
rule, the shortfall is a known gap in the WEP that proposed the rule.

A `wado` code block quotes an e2e fixture, named in an HTML comment before it
(`<!-- fixture: name.wado -->`). `mise run check-spec-examples` holds this, and
[WEP: Spec Examples Quote Fixtures](./wep-2026-09-26-spec-examples.md) says
what it asks of the block and the fixture.

A change that settles a rule writes it into the specification in the same
change. A file stays readable in one sitting; an area that outgrows that splits
into two files.

## WEP: Wado Evolution Proposals

A WEP is a proposal: the problem, the design decided for it, and the work to
get there. It covers user-visible features and compiler architecture alike.

Filename: `docs/wep-YYYY-MM-DD-{feature-name}.md`

- Title: Short description of the proposal
- Context: Background and problem statement
- Decision: What was decided and why
- Roadmap: What will be done, in order
- Known gaps: What is missing, whether or not it will be closed

A WEP keeps its history. Alternatives weighed, how the design changed, and the
checklist and roadmap entries that landed stay in it.

Once a design settles, its rules move to the specification, and the WEP stops
being where a reader looks a rule up. Where a WEP and the specification
disagree, the specification holds.

Adding or changing a language feature is the human's call, to adopt and to
refuse alike. Propose it and wait. Recording a feature that already exists is
not that call, whoever wrote it.

What an adopted decision already settles is not a second decision. Write out
what follows from it — the mechanism it implies, the invariant it rests on, the
case it forces — and say which decision it follows from where that is not
obvious. A choice the adopted one leaves open is a gap, however small.

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

## Index

@README.md

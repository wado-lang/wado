# Research: Language Survey Rubric

A fixed set of axes for evaluating an emerging language, so each survey reuses
the previous one's structure instead of starting from prose. Filled surveys are
named `research-language-survey-<name>.md`.

The goal is never a verdict on the other language. It is what Wado learns, of
which adopting something is only the narrowest kind. Reading a row for whether
it produces a decision is how a survey ends up skipping the part that would have
changed how a problem is seen, so the closing section has a place for an
understanding that carries no action.

## Rules

### Measurable only

Every axis names what to run or what file to open. An axis answered by reading
the README and forming an impression costs a full day on the next language and
produces nothing comparable.

### Claim and reality are separate columns

Record what the project says, then what the code does, in different columns.
Merging them turns the survey into an aggregation of marketing. The gap between
the two columns is usually the finding.

### Unimplemented is not rejected

A missing feature is either "not built yet" or "decided against". The first
resolves with time and says nothing about the design; the second is the design.
Every surface axis carries this column.

Implementation maturity is not itself an axis. It appears once, as a context
line in the header, and is never scored.

### Say how you looked

There are four ways to fill a row and they are not equally strong: a count from
a command, a statement read out of a document, a reading of the implementation,
and an absence found by searching. The first three carry their own evidence —
the numbers, the quotation, the file. The fourth does not.

"There is no rejection record" is unfalsifiable until it says what was searched,
so say it. The header does the same at the scale of the pass, by naming what was
examined and what was left alone. A survey that lists no gaps is claiming to
have read everything.

## A. Surface

What the language lets you write.

| Axis | What to look at |
| --- | --- |
| A1 Canonicity | One way to write each concept? Enforced by the compiler, or asserted in prose? |
| A2 Type vocabulary | Invented, or taken from an external standard? |
| A3 Effects | One bit (pure/effectful), named in the type, or handler-dispatched? |
| A4 Errors | Values or exceptions; propagation explicit or implicit; error type converted implicitly at propagation; is there a doctrine for when an error is branchable? |
| A5 Concurrency | async/await, structured, or none. Deterministic across targets and machines? |
| A6 Boundary mechanisms | How does something outside the language get in? Code generation at build time, a serialization framework at run time, foreign interface import. Macros are refused nearly everywhere; ask where generation was pushed instead, and whether what it produces can be opened |
| A7 Hidden operations | Is there an inventory of what the compiler does behind your back? |

### The self-application cross-check

Every A row carries one more column: does the claim hold in the hardest program
written in the language. Such a program needs performance, needs abstraction,
and cannot opt out, so a claim that breaks there is a surface claim.

Find that program first. The floor is the standard library, which every
language has. The ceiling is a self-hosted compiler, which is the strongest
form of the check available — a claim that survives a lexer, a type checker and
a code generator has been tested against everything the language can be asked
to express.

```sh
grep -rl '<privileged prefix>' <stdlib>/ | wc -l   # numerator
ls <stdlib>/*.<ext> | wc -l                        # denominator
```

The privileged prefix is whatever that code may call and user code may not: an
intrinsic module, a raw-memory module, an FFI escape. A public API file whose
functions all have empty bodies means the real implementation lives in another
language or under that prefix; find it before filling the row. Read the floor
too — a typed generic floor (`Array::get: (Array[T], Int) -> T`) and an
address-based one (`load64(p + 12 + i * 8)`) score the same by ratio and are
not the same finding.

An A row with only the claim column filled is not a finding.

Count the implementations of the semantics before filling any of them. One is
the quiet case. Two — a native and a wasm backend, a linear and a gc backend —
means the language is whatever both of them do, and what holds them to the same
answer is a first-class finding rather than a detail: a contract ledger and a
three-way oracle in one surveyed language, and in the other nothing, so that
`Array::push` means one thing on each backend and the memory contract says the
two "must be reconciled or documented before unifying defaults". Half of what
looks like governance zeal is the price of a second implementation.

Where the reality column turns out to be a defect, record it. The point is not
to score the other project down; it is that a broken implementation of a feature
the home language also has is the most valuable thing a survey can return.

## B. Design and governance

Why the language is the way it is, and what keeps that honest.

| Axis | What to look at |
| --- | --- |
| B1 Arbiter | Is there one sentence settling what wins when goals conflict? Record its form — a single maximized metric, or a priority order over several values — and whether it opens the philosophy document or closes it |
| B2 Accept/reject criteria | Numeric, or taste? |
| B3 The "why" axis | Roadmap records what is next and the spec records what is; is the reasoning behind a decision recorded anywhere? |
| B4 Falsifier | Does the decision template require a condition on the decision — what would retract it, or what would advance it out of "proposed"? |
| B5 Rejection record | Is there a list of designs considered and refused, with reasons and an operating rule for citing it? |
| B6 Sync gate | Does CI fail when a public claim stops being true? Over what scope? |
| B7 Self-reported violations | Does the project document where it currently breaks its own invariants, with file and line? |
| B8 Who decides | Is the human the decider, or may an agent write a decision into the record? |

B1's placement matters. A philosophy document that opens with the arbiter
derives everything from it; one that arrives at it in the closing section is
recording what was learned, not what was chosen.

B6 must record scope, not just presence. A gate over numbers and contracts
leaves prose unguarded, and prose is where a language's promises about its own
surface usually live.

Every B row takes four states, not two. A mechanism is present, or practised
without being written down, or refused with a doctrine, or absent. The middle
two are the ones a search for documents will miss. Refusal is a decision — the
same distinction the surface axes draw between unimplemented and rejected, so
record the doctrine when there is one, because two projects can lack the same
mechanism for opposite reasons. Practice is a rule that exists and has never
been stated: Wado's specification ranked itself against 132 WEPs through forty
links reading "see the WEP for rationale" and no sentence anywhere saying so,
which is not the same finding as having no rule, and is closed by writing a
paragraph rather than by building anything.

## C. Spin-off value

> If this project stopped tomorrow, what would still be worth having?

| Kind | What it is |
| --- | --- |
| C1 Artifact | A program that does a job — a compiler, a parser generator, a renderer |
| C2 Method | A technique reusable elsewhere — a way to decide, measure, or record |
| C3 Proof | Machine-checked theorems, extracted checkers |
| C4 Corpus | Reusable data — fixtures, benchmark suites, grammars |

Each entry carries two more columns.

- Externality: usable without adopting the language?
- Contender: what incumbent does it compete with, and does it win?

The language's own toolchain is not C1. A compiler, formatter, LSP, or editor
plugin for a language nobody uses is worth nothing, which is what the question
at the head of this section asks. A self-hosted compiler is the strongest entry
in the A cross-check and belongs there; counting it twice flatters the project.

C is not a proxy for maturity. A young project can have a thick C2 and C3 and an
empty C1; that is a choice about where surplus effort went, and it is visible
from the first month. C should follow from B1 — a project that named a
measurable arbiter tends to build measurement apparatus, one that named a
platform tends to build applications on it. When C and B1 disagree, that is the
finding.

Read C1 against A6 before reading it against anything else. A language with no
way for a foreign grammar, IDL, or wire format to enter it can only be used to
build its own parts, so an empty C1 beside an absent A6 is one finding rather
than two: nothing external could have been built, whatever the effort. The
artifacts that fill C1 tend to be a boundary mechanism's first customer.

## What this rubric cannot see

The instrument is document-driven. Most rows are filled by reading what a
project wrote about itself, and the self-application cross-check is the only one
that can contradict a document. In both surveys so far it produced the largest
finding, which is one measurement standing against a dozen readings.

Two failures follow. A project that documents itself poorly reads as absent
where it is only quiet, and one that documents itself well reads as present on a
mechanism nobody ran. Prefer an axis that can be counted, and where a row rests
on a document alone, say so.

There are no scores. A 1–5 ladder works for one language measured over time,
where the arbiter is fixed and the question is which way the number moved.
Across languages it prices a decision taken under one arbiter against another's,
which is the comparison this rubric exists to avoid.

## Recording template

```markdown
# Research: Language Survey — <name>

`<repo>` — <n> commits / first <date> / surveyed at <sha> <date>

Arbiter: "<one sentence, quoted>" (opens / closes the philosophy document)

Examined: <what was read or run>. Not examined: <what was left>.

## A. Surface

| Axis | Claim | Reality | Holds in self-application | Unimplemented / Rejected |

## B. Design and governance

| Axis | Present / Practised / Refused / Absent | Scope or doctrine |

## C. Spin-off value

| Kind | What | Externality | Contender | Wins |

## For Wado

- Learned:
- Take:
- Refuse (why):
- Hold:
```

Learned comes first because it is the largest of the four and the easiest to
skip: what the survey changed about how a problem is seen, whether or not
anything follows from it. A survey whose closing section is only a shopping list
was read for procurement.

Take carries open work and nothing else, so every box in it is unchecked.
Anything settled the other way goes under Refuse with its reason — including
what turned out to be already present, and what was checked and found
unnecessary. A survey re-read a year on has to show which candidates were
looked at and declined, not only which were adopted.

## Surveys

- [Almide](./research-language-survey-almide.md)
- [vibe](./research-language-survey-vibe.md)

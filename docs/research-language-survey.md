# Research: Language Survey Rubric

A fixed set of axes for evaluating an emerging language, so each survey reuses
the previous one's structure instead of starting from prose. Filled surveys are
named `research-language-survey-<name>.md`.

The goal is never a verdict on the other language. It is the last section of
every survey: what Wado takes, what it refuses, and why.

## Rules

Three rules decide whether an axis belongs here.

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

Implementation maturity is not an axis. It appears once, as a context line in
the header, and is never scored.

## A. Surface

What the language lets you write.

| Axis | What to look at |
| --- | --- |
| A1 Canonicity | One way to write each concept? Enforced by the compiler, or asserted in prose? |
| A2 Type vocabulary | Invented, or taken from an external standard? |
| A3 Effects | One bit (pure/effectful), named in the type, or handler-dispatched? |
| A4 Errors | Values or exceptions; propagation explicit or implicit; error type converted implicitly at propagation; is there a doctrine for when an error is branchable? |
| A5 Concurrency | async/await, structured, or none. Deterministic across targets and machines? |
| A6 Code generation | Macros are refused nearly everywhere. Where was generation pushed instead? |
| A7 Hidden operations | Is there an inventory of what the compiler does behind your back? |

### The stdlib cross-check

Every A row carries one more column: does the claim hold in the language's own
standard library. The stdlib is the language's harshest client — it needs
performance, it needs abstraction, and it cannot opt out. A claim that breaks
there is a surface claim.

```sh
grep -rl '<privileged prefix>' <stdlib>/ | wc -l   # numerator
ls <stdlib>/*.<ext> | wc -l                        # denominator
```

The privileged prefix is whatever the stdlib is allowed to call that user code
is not: an intrinsic module, a raw-memory module, an FFI escape. A public API
file whose functions all have empty bodies means the real implementation lives
in another language or under that prefix; find it before filling the row.

An A row with only the claim column filled is not a finding.

## B. Design and governance

Why the language is the way it is, and what keeps that honest.

| Axis | What to look at |
| --- | --- |
| B1 Arbiter | Is there one sentence naming what the project maximizes at the cost of everything else? Does it open the philosophy document or close it? |
| B2 Accept/reject criteria | Numeric, or taste? |
| B3 The "why" axis | Roadmap records what is next and the spec records what is; is the reasoning behind a decision recorded anywhere? |
| B4 Falsifier | Does the decision template require "what would retract this"? |
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

C is not a proxy for maturity. A young project can have a thick C2 and C3 and an
empty C1; that is a choice about where surplus effort went, and it is visible
from the first month. C should follow from B1 — a project that named a
measurable arbiter tends to build measurement apparatus, one that named a
platform tends to build applications on it. When C and B1 disagree, that is the
finding.

## Recording template

```markdown
# Research: Language Survey — <name>

<commits> commits / first <date> / surveyed at <sha> <date>

Arbiter: "<one sentence, quoted>" (opens / closes the philosophy document)

## A. Surface

| Axis | Claim | Reality | Holds in stdlib | Unimplemented / Rejected |

## B. Design and governance

| Axis | Present | Scope |

## C. Spin-off value

| Kind | What | Externality | Contender | Wins |

## For Wado

- Take:
- Refuse (why):
- Hold:
```

## Surveys

- [Almide](./research-language-survey-almide.md)

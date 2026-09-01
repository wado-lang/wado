# Research: Language Survey Rubric

A fixed list of questions to ask about a new language. Using the same list every
time means each survey can reuse the last one's shape instead of being invented
from scratch. Filled surveys are named `research-language-survey-<name>.md`.

The point is not to judge the other language. The point is what Wado learns.
Adopting something is only one kind of learning, and the smallest kind. If you
read each row asking "does this give us a decision?", you will skip the parts
that would have changed how you see a problem. So the closing section has a
place for things you now understand but will not act on.

## Rules

### Only ask what can be measured

Every question says what to run, or what file to open. A question you answer by
reading the README and forming an impression will cost a day on the next
language, and the two answers will not be comparable.

### Keep the claim and the reality apart

Write down what the project says. Then write down what the code does. Put them
in different columns. If you merge them you are just collecting marketing. The
gap between the two columns is usually the interesting part.

### "Not built yet" is not "decided against"

A missing feature is one or the other. "Not built yet" will change with time and
tells you nothing about the design. "Decided against" is the design. Every
surface question has a column for which one it is.

How finished the implementation is does not get its own question. It appears
once, as a line in the header, and is never scored.

### A survey is a snapshot

It describes one commit on one day. Nobody updates it afterwards. To survey the
project again, rewrite the whole file; git still has the old one. The date in
the header is the thing a later reader most needs to see.

There is no document that compares the surveys to each other, and there should
not be. A snapshot goes stale at one project's speed. A comparison goes stale at
the speed of all of them put together, because any one of them can move and
break a row. Worse, the rows that compare are exactly the ones people later
quote from memory. Anything a comparison would say belongs in the survey of the
language whose evidence produced it, written so it still holds after that
evidence goes stale.

### Say how you looked

There are four ways to fill in a row, and they are not equally reliable:

1. Counting something with a command.
2. Reading a statement in a document.
3. Reading the implementation.
4. Searching for something and not finding it.

The first three bring their own evidence: the number, the quotation, the file
path. The fourth brings nothing.

"There is no rejection record" cannot be checked or disproved until you say
where you looked. So say where you looked. The header does the same thing for
the survey as a whole: it lists what you examined and what you left alone. A
survey that admits no gaps is claiming to have read everything.

## A. Surface

What the language lets you write.

| Axis                   | What to look at                                                                                                                                                                                                                                                                    |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1 Canonicity          | Is there one way to write each thing? Does the compiler enforce it, or does a document merely say so?                                                                                                                                                                              |
| A2 Type vocabulary     | Did the language invent its type kinds, or take them from an outside standard?                                                                                                                                                                                                     |
| A3 Effects             | One bit (pure or effectful), named in the type, or dispatched to a handler?                                                                                                                                                                                                        |
| A4 Errors              | Values or exceptions? Is propagation written out or implicit? Does the error type get converted implicitly when it propagates? Is there a stated rule for when an error is worth branching on?                                                                                     |
| A5 Concurrency         | async/await, structured, or none? Does it give the same answer on every target and machine?                                                                                                                                                                                        |
| A6 Boundary mechanisms | How does something from outside the language get in? Look for code generation at build time, a serialization framework at run time, and foreign interface import. Almost everyone refuses macros, so ask where generation went instead, and whether you can open what it produces. |
| A7 Hidden operations   | Is there a list of what the compiler does behind your back?                                                                                                                                                                                                                        |

### The cross-check: does the claim survive real use?

Every A row gets one more column. Take the hardest program written in the
language and ask whether the claim still holds there. Such a program needs to be
fast, needs real abstraction, and cannot opt out of anything. A claim that breaks
there was never more than a surface claim.

Find that program first. Every language has a standard library, so that is the
floor. The strongest case is a compiler written in the language itself. A claim
that survives a lexer, a type checker and a code generator has been tested
against everything the language can be asked to express.

Then measure how much of that program had to reach below the language:

```sh
grep -rl '<privileged prefix>' <stdlib>/ | wc -l   # numerator
ls <stdlib>/*.<ext> | wc -l                        # denominator
```

The privileged prefix is whatever that code may call and ordinary user code may
not. It might be an intrinsic module, a raw-memory module, or an FFI escape. If
a public API file's functions all have empty bodies, the real implementation is
somewhere else — in another language, or under that prefix. Find it before
filling in the row.

Read the prefix itself too, not just the ratio. A floor of typed generic
operations like `Array::get: (Array[T], Int) -> T` and a floor of address
arithmetic like `load64(p + 12 + i * 8)` can score the same and mean completely
different things.

A row with only the claim column filled in is not a finding.

### Count the implementations before you start

Ask how many implementations of the semantics there are. One is the quiet case.

Two is not. It might be a native backend and a wasm backend, or a linear-memory
backend and a GC backend. Either way the language is now whatever both of them
do, and whatever keeps them agreeing is a major finding rather than a detail.
One surveyed language holds its two together with a contract ledger and a
three-way oracle. The other has nothing between them, and as a result
`Array::push` means one thing on one backend and something else on the other;
its own memory contract says the two "must be reconciled or documented before
unifying defaults".

So: a lot of what looks like unusual zeal about process is really the bill for
having built the thing twice.

### If the reality turns out to be a bug, write it down

This is not about scoring the other project down. A broken implementation of a
feature that Wado also has is the most useful thing a survey can bring back.

## B. Design and governance

Why the language is the way it is, and what keeps that honest.

| Axis                        | What to look at                                                                                                                                       |
| --------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1 Arbiter                  | Is there one sentence saying what wins when two goals conflict? Note its shape: a single number to maximize, or a priority order over several values. |
| B2 Accept/reject criteria   | Is there a stated bar for accepting a change, and is it a number or a judgement call?                                                                 |
| B3 The "why" record         | A roadmap says what is next and a spec says what is. Is the reasoning behind a decision written down anywhere?                                        |
| B4 Falsifier                | Does the decision template make you state a condition — what would retract the decision, or what would move it out of "proposed"?                     |
| B5 Rejection record         | Is there a list of designs that were considered and refused, with reasons, and a rule for when to cite it?                                            |
| B6 Sync gate                | Does CI fail when a public claim stops being true? Over how much?                                                                                     |
| B7 Self-reported violations | Does the project write down where it currently breaks its own rules, with file and line?                                                              |
| B8 Who decides              | Does a human decide, or may an agent write a decision into the record?                                                                                |

Where B1 sits in the document matters. If the philosophy document opens with the
arbiter, everything else was derived from it. If the arbiter only turns up in
the closing section, the document is recording something learned along the way
rather than something chosen at the start.

For B6, record how much the gate covers, not just that one exists. A gate over
numbers and contracts still leaves prose unchecked, and prose is usually where a
language makes promises about its own surface.

Each B row has four possible answers, not two:

- Present. The mechanism exists and is written down.
- Practised. The rule is really followed but nobody has stated it.
- Refused. There is no mechanism, on purpose, and the reason is written down.
- Absent. There is nothing.

Searching for documents will find the first and the last and miss the two in the
middle. Both matter. A refusal is a decision, and two projects can be missing the
same mechanism for opposite reasons, so record the reasoning when there is any.
Practice matters because it changes what the fix is. Wado's specification had
already settled its rank against its own WEPs, through link after link saying
"see the WEP for rationale", with no sentence anywhere stating the rule. That is
not the same as having no rule, and it is fixed by writing one paragraph rather
than by building anything.

## C. Spin-off value

> If this project stopped tomorrow, what would still be worth having?

| Kind        | What it is                                                                    |
| ----------- | ----------------------------------------------------------------------------- |
| C1 Artifact | A program that does a job: a compiler, a parser generator, a renderer         |
| C2 Method   | A technique someone else could use: a way to decide, to measure, or to record |
| C3 Proof    | Machine-checked theorems, extracted checkers                                  |
| C4 Corpus   | Reusable data: fixtures, benchmark suites, grammars                           |

Each entry gets two more columns:

- Externality: can someone use this without adopting the language?
- Contender: what does it compete with, and does it win?

The language's own toolchain does not count as C1. A compiler, formatter, LSP or
editor plugin for a language nobody uses is worth nothing, and that is exactly
what the question at the top of this section asks. A compiler written in its own
language is the strongest entry in the A cross-check instead. Counting it in both
places makes the project look better than it is.

C is not a stand-in for how mature a project is. A young project can have a
thick C2 and C3 and an empty C1. That is a choice about where the spare effort
went, and it is visible from the first month. C should follow from B1: a project
that picked a measurable arbiter tends to build measuring equipment, and one
that picked a platform tends to build applications on it. If C and B1 point in
different directions, that itself is the finding.

Read C1 next to A6 before reading it next to anything else. If nothing outside
the language can get in — no foreign grammar, no IDL, no wire format — then the
only thing you can build with it is more of itself. An empty C1 next to an
absent A6 is one finding, not two: nothing external could have been built, no
matter how much effort went in. The programs that fill C1 are usually the first
customer of some boundary mechanism.

## What this rubric cannot see

This rubric mostly reads what a project wrote about itself. Only the
cross-check can contradict a document, which puts one measurement against a
dozen readings.

So far it has contradicted a document once, and that was the biggest finding of
that survey. In the other survey it confirmed the claims instead, and the biggest
finding came from a document in which the project reported its own defect. That
is not something this rubric can rely on a project doing.

Two failures follow from being document-driven:

- A project that documents itself poorly looks like it is missing things, when
  it is only quiet.
- A project that documents itself well looks like it has mechanisms that nobody
  has ever run.

So prefer a question you can count, and where a row rests on a document alone,
say so.

There are no scores. A 1-to-5 scale works when you measure one language
repeatedly over time: the arbiter stays fixed and the only question is which way
the number moved. Across different languages it would put a price on a decision
made under one arbiter using someone else's, which is the comparison this rubric
exists to avoid.

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

Learned comes first because it is the biggest of the four and the easiest to
skip. It holds whatever the survey changed about how you see a problem, whether
or not anything follows from it. If the closing section is only a shopping list,
the survey was read for procurement.

Write each entry as the lesson, and put the observation underneath as dated
evidence. Observations expire: the bug gets fixed, the backend ships, the gap
closes. An entry written as an observation expires with it. The same entry
written as a lesson survives the fix. This is the only part of a snapshot meant
to outlive its own date.

Take holds open work and nothing else, so every box in it is unchecked. Anything
that got settled the other way moves to Refuse with its reason. That includes
things that turned out to already exist, and things you checked and found
unnecessary. A year later, the survey has to show which candidates were looked at
and put down, not only which were picked up.

## Surveys

- [Almide](./research-language-survey-almide.md)
- [vibe](./research-language-survey-vibe.md)

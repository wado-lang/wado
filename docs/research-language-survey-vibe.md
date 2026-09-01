# Research: Language Survey — vibe

Survey against [the rubric](./research-language-survey.md).

`mizchi/vibe-lang` — 5395 commits / first 2026-02-03 / surveyed at f7cb4d068
2026-08-31

Arbiter: "When they conflict, the order is never be silently wrong > honesty of
representation > pleasantness of the surface syntax." — opens the design policy
in both `README.md` and `AGENTS.md`.

An effect-typed functional language in the Rust / MoonBit / Koka / Verse
lineage, compiled to wasm, with a selfhost-only compiler: parser, checker, and
codegen are written in vibe and built from a sha256-pinned seed. The original
MoonBit host was retired outright.

The arbiter is not a maximized metric but a lexicographic order over three
values, and it is carried into operations: issue triage ranks P0 = silently
wrong above P1 = crashes.

Examined: `README.md`, `AGENTS.md`, `docs/vibe.md`, roughly a fifth of the
2,646-line cheatsheet, `docs/adr.md`, `docs/spec/decisions.md`,
`docs/spec/stable-surface.md`, `docs/pl-survey-2026-07.md`,
`eval/lang-review/rubric.md` and its latest findings, `formal/README.md`,
`docs/cli-commands.md`, the `lib/` and `scripts/` trees by listing and count,
and `lib/@vibe/builtin/` by reading. Not examined: the module system oracle and
the content-addressed package and result-cache design, which is the most
distinctive thing here and unread; the concurrency ADRs behind A5; the Perceus
work; the two wasm backends; the Vibe Book.

## A. Surface

| Axis                   | Claim                                                                      | Reality                                                                                                                                                                                                                                                                                                                                                                                                                                                                                | Holds in self-application                                                      | Unimplemented / Rejected                                                                                                            |
| ---------------------- | -------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| A1 Canonicity          | "One concept, one spelling"                                                | Mechanized, not asserted: a gate verifies documented signatures against the compiler, another enforces the canonical call form, doctest compiles every `vibe` block in the docs. A block that must not compile is marked `vibe skip` and carries a `doctest-skip:` comment giving the reason, so a "do not write this" example stays in the document without exempting the gate. `AGENTS.md` marks decided-but-not-landed rules inline and warns not to read them as today's behaviour | Yes                                                                            | Landing: structural `==` in every context, constructor-polymorphic pipeline combinators, `Exception` over `Error` at the 1.0 freeze |
| A2 Type vocabulary     | Own surface, representations chosen to match wasm and WIT without friction | A value is a tagged i64; `String` is a byte string indexed by byte offset (ADR-0098, "semantics that match what the memory actually is"); what may cross a WIT boundary is decided by nominal rules (ADR-0089)                                                                                                                                                                                                                                                                         | Yes                                                                            | —                                                                                                                                   |
| A3 Effects             | Row-polymorphic effect types with handlers; capabilities ride the row      | `fn main with Console`, `with Exception + Fs`, `handle { … } with Exception[String] { … }`. Deno-style permissions cross Koka-style effects: authority is fixed once in the earliest phase and immutable while running; `--allow-*` const-folds and dead-code-eliminates ungranted capabilities, and the emitted binary declares the wasm feature level it needs. The handler lowering is replay and it corrupts values — see below                                                    | Partly: the wasm-gc backend implements no handler at all beyond a `Throw` stub | —                                                                                                                                   |
| A4 Errors              | Failure travels in the effect row, not in a return-type wrapper            | `fn safe_div(a: Int, b: Int) -> Int with Exception[String]` — the success value flows straight through, and `handle` discharges the row at one site instead of unwrapping per call. The checked Error policy is the adopted static rule and is formalized in Lean                                                                                                                                                                                                                      | Yes                                                                            | Rejected: ambient Error, kept in the Lean model as a negative witness                                                               |
| A5 Concurrency         | Structured, shared-nothing                                                 | `TaskGroup` plus `Send`/region checks. Async syntax exists behind `--unstable-async`. Continuations designed against wasm-gc typed reference lanes, stack switching today, JSPI as an alternate backend                                                                                                                                                                                                                                                                                | Yes                                                                            | Real threads deferred, but the representation is chosen for them                                                                    |
| A6 Boundary mechanisms | No macros                                                                  | None a user can invoke: no generation command, no plugin, no build hook among the 25 user-facing CLI commands. `derive` is the only generation and its set is fixed — `Eq`, `Ord`, `Show`, `Hash`, `Default`, none of them serialization. Values cross the boundary through a dynamic `Json::` API (`stringify: (Any) -> String`), with no typed mapping either way. The compiler generates WIT from declarations                                                                      | —                                                                              | —                                                                                                                                   |
| A7 Hidden operations   | Not claimed either way                                                     | Perceus reference counting is inserted, monomorphization runs, ungranted capabilities are eliminated. Each is documented in its own file; nothing under `docs/` names an inventory of them. The cheatsheet's measured-pitfalls section covers part of the ground from the other side, recording observed behaviour rather than compiler steps                                                                                                                                          | —                                                                              | Gap, not a decision                                                                                                                 |

### The self-application cross-check

There are two implementations of the semantics here too, and nothing holds them
to one answer.

The linear backend is the production default, and the gc backend cannot be
reached from the compile CLI at all. So the disagreement between them does not
hurt yet — which is also why the bill has gone unpaid. `Array::push` is in-place
and growable on one backend and fixed-size on the other, and the memory contract
writes the debt down instead of gating against it. In the other surveyed
language, two backends of equal standing produced a contract ledger, a fixture
corpus and a three-way oracle.

On the axis that does carry weight, vibe applies the strongest form of this
check there is: the compiler is written in the language.

| Measure                                                                    | Value                                                        |
| -------------------------------------------------------------------------- | ------------------------------------------------------------ |
| vibe under `lib/`                                                          | 974 files, 360,831 lines                                     |
| the compiler itself (`lib/@vibe/compiler/`)                                | 268,463 lines, of which 55,388 are `_test.vibe`              |
| Rust in the tree (`runtime/`, `bootstrap/`)                                | 3 files, 5,562 lines — the wasmtime runner, not the compiler |
| stdlib files touching raw memory (`builtin`, `core`, `json`, `http`, `fs`) | 0 of 132                                                     |

The builtin floor is typed and generic rather than address-based —
`Array::get: (Array[T], Int) -> T`, `ArrayBuilder::push`, `Map::set` — and
`docs/builtin_contract_table.generated.md` records each builtin's signature and
effect requirement. Library code above it reads as ordinary vibe:

```vibe
export fn zip[A, B](xs: Array[A], ys: Array[B]) -> Array[(A, B)] { … }
```

Generics reach the implementation, not just the signature. A lexer, parser,
CST, desugarer, type checker, monomorphizer, Perceus pass, two codegen
backends, an optimizer, an incremental build layer, a formatter, and a WIT
generator are all written this way.

## B. Design and governance

| Axis                        | Present                               | Scope                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| --------------------------- | ------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1 Arbiter                  | Yes                                   | A lexicographic tiebreak order rather than a maximized metric, stated at the top of the design policy and encoded in triage priorities                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
| B2 Accept/reject criteria   | Partial                               | The tiebreak order settles conflicts; what it cannot settle becomes an issue with three-axis triage labels. Not numeric                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                     |
| B3 The "why" axis           | Refused, with a doctrine              | `docs/adr.md` records only what was decided; rationale goes to `git log` and the issue thread. The same stance as Wado's WEP rule, reached independently                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| B4 Falsifier                | Partial, inverted                     | Not "what would retract this" but "`proposed` must say what would make it `accepted`" — and a `proposed` entry that has not moved in a release cycle is deleted as "a decision nobody made"                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| B5 Rejection record         | Refused, with a doctrine              | "Documents rot; delete them." A status banner is "a document admitting it already failed at its job". A `superseded` row survives only while still cited as the road not taken; otherwise it is folded into the successor and deleted. An ADR whose subject no longer exists in the tree "does not become history by sitting there; it becomes a false statement about the current build"                                                                                                                                                                                                                                                                                   |
| B6 Sync gate                | Yes, the widest of the three surveyed | 49 `check_*.sh` gates, many with `_test.sh` companions, plus `check_gate_registry.sh` and `check_gate_self_tests.sh` — gates over the gates. Scope reaches prose-adjacent surfaces: doctest over every `vibe` block in the docs, cheatsheet signatures against the checker's own builtin table, cited file paths, example typechecking, book link and ordering checks, and translation output parity. The gates are placed where the previous gate is blind — the signature one exists because "doctest only compiles its ```vibe blocks, it is blind to a table", and it also holds one prose paragraph to naming exactly the documented non-builtins, no more and no less |
| B7 Self-reported violations | Yes                                   | `docs/known-issues.md`, plus decided-but-not-landed rules marked inline in `AGENTS.md` where a reader would otherwise take them for current behaviour                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| B8 Who decides              | No rule                               | Not stated                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                  |

B3 and B5 are the interesting rows, because they are refusals with reasons
rather than things nobody got round to.

vibe diagnoses exactly the failure this survey found in Almide — a gate that
covers code and is blind to prose — and answers it by deleting the redundant
surface instead of building a gate for prose. `docs/language-tour/` was removed
for precisely that reason. Doctest compile-checked its code blocks and could not
see that the prose around them still called `String` a UTF-16 string, long after
ADR-0098 made it a byte string, and still named `index.vibei` as the package
boundary after ADR-0070 replaced it.

`docs/spec/stable-surface.md` records the stable surface, and is honest about
not yet being in force. The freeze takes effect at the `0.1.0` tag, and until
then, in its own words, "this document describes the surface being frozen, not a
promise already made".

The specification opens by declaring its own authority instead of assuming it.
It says which document is canonical for surface syntax, which for the behaviour
outside syntax, and which for package boundaries and pinning. It also says that
anything marked future, proposal or draft is non-normative.

Then it splits the language a second way: what the standard tutorial teaches,
against what is documented but deliberately left out of it. That is a statement
about which subset a reader should write from, and it is a different subset from
the one that is frozen.

### The handler lowering, and what it costs

`handle` is compiled by replay. The body is wrapped in a wasm `loop`. A `perform`
unwinds to the handler, recording resolved resume values into a fixed 128 KB
region on the way. Then `resume(v)` appends to that memo and re-runs the body
**from the top**, skipping the performs it has already resolved.

Only a perform's return value gets memoized. Everything else in the body runs
again on every resume.

Their own review round measured what that does. A body that performs twice
prints its first line three times. A `let mut` accumulator reads 23 where 11 was
expected. The function returns 29 where 17 was expected. The value itself is
wrong.

What shipped as the fix was a sentence in the cheatsheet, telling authors to keep
a handle body pure until the last perform. No fixture pins the defect. There is
also a hard ceiling: 128 KB divided by 8 bytes is 16,382 performs per handle. The
constant is duplicated across three files, and shrinking it once corrupted the
heap.

ADR-0076 replaces all of it with Koka's generalized evidence passing. The
design's first move is reuse: vibe already passes trait dictionaries as implicit
witness parameters for generic method calls, so the evidence vector is built on
that same mechanism instead of a second one.

Two things in the write-up are worth as much as the decision itself.

First, it corrects the premise of the issue that asked for it. The issue assumed
there was a tail-resumptive fast path to generalize; that path only ever existed
in the retired MoonBit host. So this is introducing a mechanism, not generalizing
one.

Second, phase 1 is to pin the existing corruption as a regression fixture
_before_ fixing anything, so that "fixed" can later be proved rather than
asserted.

The plan then shrank once they investigated. A CPS runtime was scheduled for
phase 3 and turned out to be unnecessary: the checker already rejects non-tail
and multi-shot resume, which by the Xie and Leijen definition makes every handler
tail-resumptive already. So the remaining work is just widening how much the
evidence pass covers statically. The document records that the phase evaporated.

### Backends, and where wasm-gc actually is

The gc backend is not what its name suggests. It turns on the Wasm-GC feature
set, but it does not emit `struct.new` or `array.new` for user values. Tuples,
structs, constructors and closures all live in guest linear memory behind a bump
allocator, so there is nothing for a tracing collector to reclaim.

It is also unreachable from the compile CLI: `compile --wasm-gc` throws. The only
way in is through test and bench environment variables, for cases with no host
imports.

The production default is linear memory with Perceus reference counting.
Reclamation is eager and deterministic. Cycles leak permanently. The compiler's
own self-build is pinned away from RC and back to bump, for performance — RC
costs it roughly 1.7× in wall clock and 2.9× in output size.

The two backends do not agree on what the language means. `Array::push` is
in-place and growable on linear, and fixed-size with a rebound local on gc. The
memory contract records that this "must be reconciled or documented before
unifying defaults".

The same document says what happens once the gc lane emits real GC allocations.
Eager RC against lazy GC is a fundamental difference, and the plan for keeping
observable behaviour identical across it is a differential test rather than an
argument.

### Oracle documents

The module system is written up in a genre that neither Wado nor this rubric's
axes account for.

`docs/module-system-oracle.md` declares itself the executable canon of its ADR. A
Lean model under `formal/VibeFormal/Module/` decides package boundary, ownership,
visibility and hash input. The compiler's loader then refines that judgement onto
a real filesystem. The prose is the readable form of the model, and it says so:
every other document touching the subject "links here and holds no explanation of
its own", and where documents from different generations disagree, this section
and the Lean model win.

Its table is the shape worth stealing. One row per observable rule, and three
columns: the Lean definition that decides it, the implementation function that
refines it, and the test that pins the pair together. So `Boundary` and
`Workspace.ownerOf` sit against `nearest_vpkg_path_fs`, `AllowedImport` against
`enforce_incoming_boundary_fs`, and `automaticallyIncludedInPackageHash` against
`collect_package_hash_source_closure_fs`.

It closes with an `Epistemic status` section. That section says what the proof
does _not_ cover — filesystem path normalization, the parser, the cache, hash
bytes and wasm codegen are none of them extracted from Lean. It says what holds
those instead: regression tests and a fixpoint gate. And it says what would close
the gap: a differential oracle that turns fixture graphs into Lean inputs, and an
inductive model of hash-closure reachability.

One decision inside the model is worth taking on its own. Transparent types
declared in a package contract are ambient inside that package. Rather than
special-case how a sibling module resolves an enum constructor, the loader writes
those types out as a real source module under `_build/`, content-keyed and
ownerless. The facade re-exports it, and siblings import it through the ordinary
mechanism.

That turns a special case into an instance of the general rule. What it prevents
is a second definition site, because two same-named enums would become separate
identities and trap at run time.

## C. Spin-off value

| Kind        | What                                                                                                                                                                                                                                                                                                  | Externality | Contender                                      | Wins |
| ----------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ---------------------------------------------- | ---- |
| C1 Artifact | None outside the toolchain, across `examples/`, `tools/`, `clients/` and `integrations/`: editor integrations (tree-sitter, VS Code, Zed), a JS/wasm embedding client, and the LSP all serve vibe itself                                                                                              | —           | —                                              | —    |
| C2 Method   | `docs/pl-survey-2026-07.md` — a PL survey that reads primary sources, tabulates implications per topic, and turns them into prioritized adoption proposals carrying landed status                                                                                                                     | High        | Reading papers without a decision trail        | Yes  |
| C2 Method   | `eval/` — five evaluation loops: `msr`, `lang-review` (rubric-based design review), `lang-bench`, `call-style`, `book-review`                                                                                                                                                                         | High        | —                                              | —    |
| C2 Method   | "Documents rot; delete them", the ADR log rules, and the gate registry with per-gate self-tests                                                                                                                                                                                                       | High        | Status banners and an append-only decision log | Yes  |
| C2 Method   | The executable book: 40 `*.vibe.md` chapters whose code blocks are compiled and run with the output embedded, and whose translations are held to identical output by a CI gate                                                                                                                        | High        | Prose documentation with untested examples     | Yes  |
| C2 Method   | The cheatsheet's measured-pitfalls section: every rule carries the date it was measured against the compiler and the fixture that pins it, and the section records where it was previously wrong                                                                                                      | High        | A "common mistakes" list written from memory   | Yes  |
| C3 Proof    | `formal/` — Lean 4 models of the effect row, the checked Error policy, capability and resource contracts, parallel scheduling, and the module system, driven by oracle scripts. Includes negative witnesses: a deliberately broken checker is modeled and shown to admit an undeclared `Fs` operation | High        | —                                              | —    |
| C4 Corpus   | 1,266 fixture and test files, 42,467 lines                                                                                                                                                                                                                                                            | Medium      | —                                              | —    |

C is thick in method and proof and empty in artifacts — the same shape as
Almide, from a different arbiter, and bounded the same way by A6. A language
whose only generation is a fixed `derive` set and whose only wire crossing is an
untyped `Json::` API gives a foreign grammar or format no route in, so what can
be built with it is what it is made of. The self-hosted compiler is not counted
here: it is worth nothing if the language stops, which is what makes it an A
cross-check rather than spin-off value.

The measured-pitfalls section states its purpose as keeping anyone from
investigating the same question twice, and two of its entries record the section
having been wrong. One had quoted a diagnostic's own enumeration as the rule;
measuring it showed that four of the shapes the message named as rejected in
fact compile, and that the two which fail do so for reasons the message never
mentioned — the message was rewritten and a measured table replaced the
enumeration. The other notes that the cheatsheet had the declaration separator
backwards in the very place that explains it.

Its lead entry shows what the format is for. A library that defines
`fn String::index_of` replaces the builtin of that name across the whole linked
program: a file that imports only some unrelated function from that library gets
the replacement, nothing is reported either way, and the two readings of one
source are indistinguishable without running it. The measured cost is 0.8 us
against the builtin and 174 us against the shadowing definition, and the answers
differ as well as the speed. A lexical scanner built to catch the hazard missed
it three ways — a raw identifier, a preceding declaration attribute, and a
declaration split across lines — from which the entry concludes that deciding
what a declaration binds is the compiler's job.

`docs/pl-survey-2026-07.md` is this document's own genre, done first, and it
surveys Almide specifically — down to naming `almide-mir/src/alias_safety.rs` as
ahead of vibe's own Perceus work, and recording that Almide's binary-size
benchmark method and its modification survival rate were both adopted, with the
issue numbers where they landed. The `eval/msr/` README states the borrowing
outright.

## For Wado

Learned:

- Declining to build what the platform is going to provide leaves the fast path
  as the only path. Wado lowers a tail-position `resume` to `return` and reaches
  for Wasm stack switching only when code follows the `resume`, so no general
  suspension machinery was ever written and none can be wrong. As of this
  survey vibe has no tail-resumptive path at all, every perform goes through
  replay, and ADR-0076 concludes that its checker's rejection of non-tail and
  multi-shot resume already makes every handler tail-resumptive — arriving,
  by a different route and later, at the lowering Wado has.
- A bet is easier to price from outside it. Wado's founding one — let the host
  own the collector, ship no runtime — reads as an obvious constraint from
  within and as an escape from a long bill once the alternative is written down.
  What both surveyed languages were paying at this date: reference counting over
  linear memory, cycles leaking permanently as a stated limitation, a compiler
  self-build pinned away from RC because it costs 1.7× wall and 2.9× size, a gc
  backend that emits no `struct.new` and disagrees with the linear one about
  what `Array::push` means. Neither had reached the target both name as primary.
- Pin the defect before fixing it, at architecture scale. ADR-0076's phase 1 is
  a regression fixture for the corruption its phase 2 removes, written so that
  "fixed" becomes provable instead of asserted. This is the red/green rule
  `AGENTS.md` already requires, applied to a migration long enough that nobody
  would otherwise remember what the old behaviour was.
- Both surveyed languages arrived independently at two documents Wado has
  neither of, which is a stronger signal than either instance alone. The first
  is a traceability table running claim → mechanism → evidence: vibe's oracle
  maps each module rule to its Lean definition, its loader function and its
  regression test; Almide's `edit-locality.md` §2 maps each language rule to the
  role it plays in enforcing L1 and the `file:line` that implements it; its
  contract ledger maps each promise to a specification section and a fixture,
  gated in both directions. The second is a boundary map declaring where the
  guarantee stops — vibe's `Epistemic status`, Almide's `proven-vs-trusted.md`.
- Wado has the parts and not the map. A long optimizer pass list, a NIR/WIR
  pipeline, EMI mutation, fuzzing and golden fixtures, and no document saying
  which claim each of those backs. The cost of not having it is already
  recorded: a tree that passed the whole e2e suite was miscompiling under `-O3`,
  and Gale caught it. A boundary map is what says out loud that the `-O3`
  pipeline was backed by e2e tests and that e2e tests do not reach it.
- A rule attached to the artifact it governs holds without a gate; a rule that
  must be recalled somewhere else needs one. Almide's mandatory falsifier is a
  heading in the template the author already has open and all twelve ADRs have
  it; its "no synonyms" rule has to surface at an unrelated keystroke months
  later and the standard library has synonyms. Neither is enforced. This is the
  test to apply before writing a rule down and expecting it to hold.
- Gates are placed where the previous gate is blind, and vibe does this three
  times: doctest compiles code blocks, so a signature gate covers the tables it
  cannot see; the deleted language tour was the surface where nothing covered
  the prose; the eighth review dimension exists because the other seven observe
  through modes that cannot see a missing diagnostic. The question that produces
  the next gate is what the current one cannot observe.

Take:

- [ ] A measured-pitfalls section for the cheatsheet, in vibe's form rather than
      Almide's. The difference is three writing rules: each entry states the
      date it was measured against the compiler, names the fixture that pins it,
      and, where an earlier version of the entry was wrong, says so. A list
      written from memory decays into folklore; this one cannot, because every
      row can be re-run.
- [ ] Gates over documentation, not only over code. vibe compiles every code
      block in its docs against the current compiler, checks documented
      signatures against the compiler, and verifies that file paths cited in
      docs still exist. Wado's cheatsheet and spec have no such gate, and the
      third of these is a shell script's worth of work. The mechanism that makes
      the first one tractable is a skip marker that requires a reason, so an
      example of what not to write stays in the document without exempting the
      gate silently.
- [ ] Gate self-tests and a gate registry. A check script with no test is a
      claim nobody checked; vibe pairs most of its gates with a `_test.sh` and
      keeps a registry gate over the set.

Refuse:

- Errors in the effect row. Wado already carries effects in the signature, and
  moving failure there too would collapse the distinction between "this
  function touches the outside world" and "this function can fail". Almide
  reached the opposite conclusion deliberately — fallibility and effect are
  orthogonal axes — and Wado's split of `Result` from `with` agrees.
- `#zero_alloc`, the attribute making any heap allocation in a function or a
  transitively called one a compile error, in three modes. It is not clear whom
  the assertion is addressed to. A caller cannot act on it, and the property it
  asserts is decided by the ownership analysis and the optimizer rather than by
  the annotated code, so the annotation breaks for reasons its own body did not
  cause. [Optimizer Remarks](./wep-2026-06-03-optimizer-remarks.md) reports the
  same facts to the reader who can act on them.
- The `proposed` advancement rule. `docs/CLAUDE.md` already requires an
  unfinished mechanism to be a "Known gap" stating what is missing and what
  closing it takes, which is the same obligation.
- An authority header on `spec.md` — taken and landed while this survey was
  open, so it is no longer work to schedule. `spec.md` now opens by saying that
  it is normative, that a disagreement with the implementation is a defect
  belonging to whichever side is wrong, and where the WEPs, the cheatsheet and
  the generated stdlib pages stand against it.
- Citing vibe's program-wide shadowing measurement in
  [`wado lint`](./wep-2026-08-31-wado-lint.md) as evidence that a check must
  resolve declarations rather than spellings.
  [Declaration Identity](./wep-2026-08-12-declaration-identity.md) is that
  evidence, recorded from Wado's own name-collision history.

Hold:

- The rejection record proposed in the Almide survey. vibe refuses it on a
  stated doctrine — a superseded entry becomes a false statement about the
  current build — and vibe's position is closer to Wado's existing WEP rule
  than Almide's is. The two surveys disagree, so this is a real design choice
  rather than an obvious import. The narrow version that survives both
  objections: record the rejection, never the superseded design, and delete a
  rejection only when the language's direction changes.

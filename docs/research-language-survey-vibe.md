# Research: Language Survey — vibe

Survey against [the rubric](./research-language-survey.md).

5395 commits / first 2026-02-03 / surveyed at f7cb4d068 2026-08-31
([mizchi/vibe-lang](https://github.com/mizchi/vibe-lang))

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

## A. Surface

| Axis | Claim | Reality | Holds in self-application | Unimplemented / Rejected |
| --- | --- | --- | --- | --- |
| A1 Canonicity | "One concept, one spelling" | Mechanized, not asserted: `check_cheatsheet_signatures.sh` verifies documented signatures against the compiler, `check_canonical_iterator_calls.sh` enforces the canonical call form, doctest compiles every `vibe` block in the docs. `AGENTS.md` marks decided-but-not-landed rules inline and warns not to read them as today's behaviour | Yes | Landing: structural `==` in every context (ADR-0097), constructor-polymorphic pipeline combinators (ADR-0110), `Exception` over `Error` at the 1.0 freeze |
| A2 Type vocabulary | Own surface, representations chosen to match wasm and WIT without friction | A value is a tagged i64; `String` is a byte string indexed by byte offset (ADR-0098, "semantics that match what the memory actually is"); what may cross a WIT boundary is decided by nominal rules (ADR-0089) | Yes | — |
| A3 Effects | Row-polymorphic effect types with handlers; capabilities ride the row | `fn main with Console`, `with Exception + Fs`, `handle { … } with Exception[String] { … }`. Deno-style permissions cross Koka-style effects: authority is fixed once in the earliest phase and immutable while running; `--allow-*` const-folds and dead-code-eliminates ungranted capabilities, and the emitted binary declares the wasm feature level it needs | Yes | — |
| A4 Errors | Failure travels in the effect row, not in a return-type wrapper | `fn safe_div(a: Int, b: Int) -> Int with Exception[String]` — the success value flows straight through, and `handle` discharges the row at one site instead of unwrapping per call | Yes | Checked Error policy is the adopted static rule; formalized in Lean, with ambient Error retained as a rejected comparison witness |
| A5 Concurrency | Structured, shared-nothing | `TaskGroup` plus `Send`/region checks. Async syntax exists behind `--unstable-async`. Continuations designed against wasm-gc typed reference lanes, stack switching today, JSPI as an alternate backend | Yes | Real threads deferred, but the representation is chosen for them |
| A6 Code generation | No macros | `derive(Eq, Show)` on declarations; the executable book generates its own output | — | — |
| A7 Hidden operations | Not claimed either way | Perceus reference counting is inserted, monomorphization runs, ungranted capabilities are eliminated. Each is documented in its own file; there is no single inventory | — | Gap, not a decision |

### The self-application cross-check

vibe applies the strongest available form of this check: the compiler is
written in the language.

| Measure | Value |
| --- | --- |
| vibe under `lib/` | 974 files, 360,831 lines |
| the compiler itself (`lib/@vibe/compiler/`) | 268,463 lines, of which 55,388 are `_test.vibe` |
| Rust in the tree (`runtime/`, `bootstrap/`) | 3 files, 5,562 lines — the wasmtime runner, not the compiler |
| stdlib files touching raw memory (`builtin`, `core`, `json`, `http`, `fs`) | 0 of 132 |

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

| Axis | Present | Scope |
| --- | --- | --- |
| B1 Arbiter | Yes | A lexicographic tiebreak order rather than a maximized metric, stated at the top of the design policy and encoded in triage priorities |
| B2 Accept/reject criteria | Partial | The tiebreak order settles conflicts; what it cannot settle becomes an issue with three-axis triage labels. Not numeric |
| B3 The "why" axis | Refused, with a doctrine | `docs/adr.md` records only what was decided; rationale goes to `git log` and the issue thread. The same stance as Wado's WEP rule, reached independently |
| B4 Falsifier | Partial, inverted | Not "what would retract this" but "`proposed` must say what would make it `accepted`" — and a `proposed` entry that has not moved in a release cycle is deleted as "a decision nobody made" |
| B5 Rejection record | Refused, with a doctrine | "Documents rot; delete them." A status banner is "a document admitting it already failed at its job". A `superseded` row survives only while still cited as the road not taken; otherwise it is folded into the successor and deleted. An ADR whose subject no longer exists in the tree "does not become history by sitting there; it becomes a false statement about the current build" |
| B6 Sync gate | Yes, the widest of the three surveyed | 49 `check_*.sh` gates, many with `_test.sh` companions, plus `check_gate_registry.sh` and `check_gate_self_tests.sh` — gates over the gates. Scope reaches prose-adjacent surfaces: doctest over every `vibe` block in the docs, cheatsheet signatures against the compiler, cited file paths, example typechecking, book link and ordering checks, and translation output parity |
| B7 Self-reported violations | Yes | `docs/known-issues.md`, plus decided-but-not-landed rules marked inline in `AGENTS.md` where a reader would otherwise take them for current behaviour |
| B8 Who decides | No rule | Not stated |

B3 and B5 are the interesting entries, because they are refusals with reasons
rather than gaps. vibe diagnoses precisely the failure this survey found in
Almide — a gate that covers code and is blind to prose — and answers it by
deleting the redundant surface instead of building a prose gate.
`docs/language-tour/` was removed for exactly this: doctest compile-checked its
code blocks and could not see that its prose still called `String` a UTF-16
string after ADR-0098 made it a byte string, and still named `index.vibei` as
the package boundary after ADR-0070 replaced it.

The stable surface is recorded in `docs/spec/stable-surface.md` and is honest
about not yet being in force — the freeze takes effect at the `0.1.0` tag,
"until then this document describes the surface being frozen, not a promise
already made".

## C. Spin-off value

| Kind | What | Externality | Contender | Wins |
| --- | --- | --- | --- | --- |
| C1 Artifact | None outside the toolchain. Editor integrations (tree-sitter, VS Code, Zed), a JS/wasm embedding client, and the LSP all serve vibe itself | — | — | — |
| C2 Method | `docs/pl-survey-2026-07.md` — a PL survey that reads primary sources, tabulates implications per topic, and turns them into prioritized adoption proposals carrying landed status | High | Reading papers without a decision trail | Yes |
| C2 Method | `eval/` — five evaluation loops: `msr`, `lang-review` (rubric-based design review), `lang-bench`, `call-style`, `book-review` | High | — | — |
| C2 Method | "Documents rot; delete them", the ADR log rules, and the gate registry with per-gate self-tests | High | Status banners and an append-only decision log | Yes |
| C2 Method | The executable book: 40 `*.vibe.md` chapters whose code blocks are compiled and run with the output embedded, and whose translations are held to identical output by a CI gate | High | Prose documentation with untested examples | Yes |
| C3 Proof | `formal/` — Lean 4 models of the effect row, the checked Error policy, capability and resource contracts, parallel scheduling, and the module system, driven by oracle scripts. Includes negative witnesses: a deliberately broken checker is modeled and shown to admit an undeclared `Fs` operation | High | — | — |
| C4 Corpus | 1,266 fixture and test files, 42,467 lines | Medium | — | — |

C is thick in method and proof and empty in artifacts — the same shape as
Almide, from a different arbiter. The self-hosted compiler is not counted here:
it is worth nothing if the language stops, which is what makes it an A
cross-check rather than spin-off value.

`docs/pl-survey-2026-07.md` deserves separate mention because it is this
document's own genre, done first. It surveys sixteen topics from primary
sources, and it surveys Almide specifically — down to naming
`almide-mir/src/alias_safety.rs` as ahead of vibe's own Perceus work, and
recording that Almide's binary-size benchmark method and its modification
survival rate were both adopted, with the issue numbers where they landed. The
`eval/msr/` README states the borrowing outright.

## For Wado

Take:

- [ ] Gates over documentation, not only over code. vibe compiles every code
      block in its docs against the current compiler, checks documented
      signatures against the compiler, and verifies that file paths cited in
      docs still exist. Wado's cheatsheet and spec have no such gate, and the
      third of these is a shell script's worth of work.
- [ ] Gate self-tests and a gate registry. A check script with no test is a
      claim nobody checked; vibe pairs most of its gates with a `_test.sh` and
      keeps a registry gate over the set.
- [ ] The `proposed` advancement rule for WEPs: a proposal must state what
      would make it accepted, and one that has not moved in a cycle is deleted
      rather than left standing. This composes with Wado's rule that only the
      human decides, and is a lighter alternative to the falsifier field
      proposed in the [Almide survey](./research-language-survey-almide.md).

Refuse:

- Errors in the effect row. Wado already carries effects in the signature, and
  moving failure there too would collapse the distinction between "this
  function touches the outside world" and "this function can fail". Almide
  reached the opposite conclusion deliberately — fallibility and effect are
  orthogonal axes — and Wado's split of `Result` from `with` agrees.

Hold:

- The rejection record proposed in the Almide survey. vibe refuses it on a
  stated doctrine — a superseded entry becomes a false statement about the
  current build — and vibe's position is closer to Wado's existing WEP rule
  than Almide's is. The two surveys disagree, so this is a real design choice
  rather than an obvious import. The narrow version that survives both
  objections: record the rejection, never the superseded design, and delete a
  rejection only when the language's direction changes.

# Research: Language Survey — Almide

Survey against [the rubric](./research-language-survey.md).

`almide/almide` — 7827 commits / first 2026-03-07 / surveyed at b900c57ed
2026-08-31

Arbiter: "Almide optimizes for minimal thinking tokens: the less an LLM has to
branch over syntax, semantics, repair strategies, or missing abstractions, the
faster, cheaper, and more reliable code generation becomes." — opens
`docs/design/DESIGN.md`.

A statically-typed language compiling to native (via Rust) and to wasm, holding
byte-identical observable output across the two as a hard invariant. Its stated
metric is modification survival rate: how often code still compiles and passes
its tests after a series of AI-driven edits.

Examined: `README.md`, `llms.txt`, `docs/CHEATSHEET.md`, the five files under
`docs/design/`, `docs/adr/README.md`, `docs/specs/edit-locality.md`,
`docs/STABILITY.md`, `demo/make-verify/README.md`,
`research/grammar-lab/REPORT.md`, and the `stdlib/`, `runtime/` and `crates/`
trees by listing, count and sampled reading. Not examined: the 1,607-line
`SPEC.md` and the rest of `docs/specs/`; the contract ledger itself, cited here
only by its count; the ADR bodies beyond their section structure and two sampled
falsifiers; what the Lean belts prove, as against whether they prove it;
`TRUST-SPINE.md`, taken from the claims made about it.

## A. Surface

| Axis                   | Claim                                                                                             | Reality                                                                                                                                                                                                                                                                                                                                                                                | Holds in self-application                                                                                                                        | Unimplemented / Rejected                                                                                                                                                                         |
| ---------------------- | ------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| A1 Canonicity          | "One name per operation, no aliases" (`DESIGN.md`); the cheatsheet lists `string.length` as wrong | Prose convention only; nothing checks it                                                                                                                                                                                                                                                                                                                                               | No — `stdlib/string_len.almd` defines `string.length` as an alias of `string.len`, and `list.len`/`list.length` both map to `almide_rt_list_len` | —                                                                                                                                                                                                |
| A2 Type vocabulary     | Invented; primitives cut to `Int`, `Float`, `String`, `Bool`, `Unit`, `Path`                      | Sized integers exist as conversion modules (`int8`…`uint64`) reached by UFCS, not as surface types. There is no external ABI the vocabulary has to mirror                                                                                                                                                                                                                              | —                                                                                                                                                | —                                                                                                                                                                                                |
| A3 Effects             | `effect fn` marks I/O; a pure fn cannot call one (E006)                                           | One bit. No effect names, no polymorphism, no handlers                                                                                                                                                                                                                                                                                                                                 | Yes                                                                                                                                              | Rejected — `REJECTED_PATTERNS.md` refuses algebraic effects, citing Gleam                                                                                                                        |
| A4 Errors              | `Result[T, E]` only; `T!` is `Result[T, String]`, `T!E` is `Result[T, E]`, `T?` is `Option[T]`    | Propagation is `!` and always explicit (ADR-0008 abolished auto-`?`); `!` never converts `E`, a mismatch stays a type error (ADR-0003)                                                                                                                                                                                                                                                 | Yes                                                                                                                                              | Rejected: exceptions, `null`, implicit conversion at propagation                                                                                                                                 |
| A5 Concurrency         | `fan`, structured, no async/await                                                                 | Deterministic by construction: `fan.race` picks the winner by least compute spent, ties by source order, same answer on every target and machine. `Compute` and `Duration` are separate types; a bare `Int` is not a time and there are no literal suffixes                                                                                                                            | Yes                                                                                                                                              | Rejected: async/await (function colouring), goroutines, exposed `Future[T]`                                                                                                                      |
| A6 Boundary mechanisms | No macros, no reflection                                                                          | None a user can invoke. The TOML and `build.rs` that `REJECTED_PATTERNS.md` names as the alternative to macros are the compiler's own build — `codegen/templates/rust.toml` is the Rust backend's syntax table, "pure formatting, no semantic logic". Values cross the boundary through `Codec`, derived by `: Codec` and producing `Value`, with JSON the only format on the far side | —                                                                                                                                                | Rejected: macros, reflection, monkey patching, and executable build scripts — `almide.toml` is declarative TOML only, because `setup.py`-style executable configuration destroys reproducibility |
| A7 Hidden operations   | Zig's "no hidden control flow" cited, then deliberately departed from                             | `docs/design/HIDDEN_OPERATIONS.md` enumerates five, each with trigger condition, implementing file, and user impact: clone insertion, runtime embedding, Perceus RC insertion, `fan` threading, and one entry recording that auto-`?` was removed                                                                                                                                      | —                                                                                                                                                | —                                                                                                                                                                                                |

A4 comes with a stated doctrine that the other axes do not have. Errors have two
jobs, and the choice of `E` is which job you are doing.

`E = String` is the reporting channel, and the default. A variant `E` is the
branching channel, and it only earns its cost inside a closed domain — a module
or package that handles that error itself. Leaving that domain demotes the
variant back to `String`, and the demotion is always visible as a `map_err`,
because there is no conversion hook that could do it quietly.

Two lints hold the doctrine up. E035 warns when code branches on an error's
message text. E036 warns when a `map_err` takes a named parameter and then drops
`${e}` from the result.

### The self-application cross-check

There are two implementations of the semantics: a Rust runtime for native, and
self-hosted `.almd` for wasm.

Almost everything under B6, and most of C4, is the bill for that. The
327-contract ledger, the 610 cross-target fixtures, the three-way oracle against
the interpreter, the differential fuzz — all of it exists to hold two
implementations to one answer. The byte-identity claim is what that buys.

The hardest program written in Almide is its own standard library. The compiler
itself is Rust, 836 files across 24 crates, so the strongest form of this check
— a compiler written in the language — is not available here. The stdlib has to
carry it alone.

232 of the 309 stdlib files, or 75%, call `prim`. The module's own header calls
it "the PRIMITIVE FLOOR: raw memory access … UNSAFE: addresses are unchecked".
In practice that means hand-written address arithmetic:

```almide
let x = prim.load64(sh + 12 + i * 8)
prim.store_str(dh + 12 + i * 8, y)
```

The public surface is 971 functions across 43 modules, and they are declared
generically with empty bodies — the `.almd` files carry 808 `@intrinsic`
annotations. The real bodies are in two other places: `runtime/rs` (42 Rust
files, 12,941 lines) for native, and the self-hosted `.almd` files for wasm.

Generics reach the declaration but not the implementation. `List[A]` is a single
generic signature sitting on top of one hand-written file per type pairing:
`list_map_s2h` (scalar source, heap result), `list_fold_hsca`,
`list_to_string_ll`, `list_filter_str`, `map_skv`, `option_to_string_oi`. The
compiler picks between them.

`list_map_s2h`'s own comment says what happens when it picks wrong:
`list.map_str` "would misread the i64 scalar slot as a String handle and corrupt
it". In total, 3,955 function definitions stand behind 971 public ones.

So three of the surface claims — no raw pointers, generics, no aliases — do not
survive contact with the language's own standard library.

## B. Design and governance

| Axis                        | Present                    | Scope                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| --------------------------- | -------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| B1 Arbiter                  | Yes                        | One sentence, opening the philosophy document; every other section derives from it                                                                                                                                                                                                                                                                                                                                                                                  |
| B2 Accept/reject criteria   | Yes, numeric               | Rejected if median MSR drops ≥ 3 points on any model, or if it introduces a second canonical form, or if it trades generation predictability for human aesthetics. Accepted if MSR measurably rises, or if it removes an error class that retry-with-diagnostic cannot repair                                                                                                                                                                                       |
| B3 The "why" axis           | Yes                        | `docs/adr/`, 12 records. Its README states the problem directly: roadmap and spec both fail to preserve why, so the same argument is replayed and a rejected proposal returns with its rationale lost                                                                                                                                                                                                                                                               |
| B4 Falsifier                | Yes, mandatory, and obeyed | Template field, with the rule "a decision whose retraction condition cannot be written is a preference, not a decision". All twelve ADRs carry it, and carry `Alternatives` too. The ones sampled hold a threshold, a named fallback position and, in ADR-0001, the experiment that will produce the number: if the calibration gate measures a spread beyond the declared band, roughly 5×, the millisecond label comes off and the units go back to dimensionless |
| B5 Rejection record         | Yes                        | `docs/design/REJECTED_PATTERNS.md`, 154 lines, with an operating rule: add on rejection with the reason, remove only on an edition-level change of direction, cite it when a PR proposes the feature. Carries a reversal in place — `??` was rejected, then reinstated by ADR-0005, and the rejection is kept as history                                                                                                                                            |
| B6 Sync gate                | Yes, partial               | `check-readme-numbers.sh` refuses a bare number in the README, `gen-claims.sh` generates the public claims block from the contract ledger, `check-contracts.sh` fails on drift. Scope is numbers and contracts. Prose is unguarded, which is how A1 leaked                                                                                                                                                                                                          |
| B7 Self-reported violations | Yes                        | `docs/specs/edit-locality.md` §3 lists eight standing violations of its own L1 invariant with mechanism and `file:line`, triaged into language bugs, backend obligations, and declared side conditions                                                                                                                                                                                                                                                              |
| B8 Who decides              | No rule                    | The ADR rules constrain a decision's quality, not its author                                                                                                                                                                                                                                                                                                                                                                                                        |

One invariant is worth naming, because it is what all the surface rules serve.
They call it L1, the "edit frame":

> Take a well-typed program and edit one definition's body without changing its
> signature. Every execution that never enters that definition must produce
> identical observable output.

`edit-locality.md` §2 maps each language rule to the role it plays in enforcing
L1: mandatory return types, no overloading, no glob imports, module-isolated
inference, no dynamically scoped handlers. §5 turns it into a gate — every
change has to answer "does this preserve L1?" before it can land.

Two of the project's rules are unchecked prose, and only one of them leaked.
Nothing enforces the mandatory falsifier, and all twelve ADRs have one. Nothing
enforces "no synonyms", and the standard library has synonyms.

The difference is where you have to remember the rule. A falsifier is a heading
in a template the author already has open, so an empty one is visible while the
decision is being written. Not adding a synonym has to occur to you months
later, at an unrelated keystroke, with nothing on screen to prompt it.

So: a rule attached to the thing it governs holds without a gate. A rule you
have to remember somewhere else needs one.

### The boundary map, and what a certificate does not buy

`docs/contracts/proven-vs-trusted.md` opens with the question an auditor asks
first about a compiler that ships proofs: which part is actually proven? It
answers before it sells anything.

> An accepted certificate proves the function is memory-safe … It proves
> **nothing** about whether the lowering picked the right semantics: a certified-
> sound function can still print the wrong string.

Under that is a table with one row per pipeline stage. Each row is marked proven,
trusted, or unqualified tool, and says what backs it. The lexer and parser are
trusted, on differential fuzz. The type checker kernel is proven in Coq. The MIR
ownership witness is "proven to be re-checkable": an untrusted producer emits a
witness and the kernel-proven checker re-verifies it. wasmtime is out of scope by
construction.

The document also names which row is the gap that matters and the issue tracking
it. And it scopes itself to one of the two wasm legs, warning the reader that
nothing in it describes bytes the other leg produced.

`TRUST-SPINE.md` states the architecture in one move. Do not prove the compiler.
Prove a tiny checker, and have the compiler emit a certificate on every build
that the checker re-verifies.

That works because building is hard and checking is cheap. The compiler is
allowed to have bugs: if it produces a wrong artifact, the attached certificate
fails to check. The only theorem needed is "if the checker accepts, the artifact
has the property", and that theorem never mentions the compiler's internals. The
trusted base drops from the whole compiler to 1,348 lines of OCaml extracted from
the proofs — a figure regenerated into the document between markers, so it cannot
drift.

What makes it worth reading is that this advocacy piece states its own limits.
There is no mechanized evaluation relation for Almide source, and no theorem of
the shape `⟦s⟧ ≈ ⟦compile(s)⟧`. The translation checker does a structural
realization check, not a semantic refinement proof. Stack balance and termination
are proven in the Rocq spine but are not extracted into the checker and not
witnessed per build. Verified extraction is called a future ratchet, not a
present fact.

Byte-for-byte agreement between the two targets is the project's headline claim,
and the document says in the same paragraph that it "is established
empirically". CI is deliberately placed outside the trusted base, as a courtesy
pre-run, so that trusting the artifact never requires trusting their
infrastructure.

The contract ledger runs the same traceability the other way. Each of its 327
entries is a named promise, carrying both the specification section it certifies
and the fixture that executes it. The gate makes that link mandatory in both
directions: a specification heading no contract cites fails, and so does a
contract citing no heading. The stated purpose is that "no claimed behaviour
rests on prose alone".

## C. Spin-off value

| Kind        | What                                                                                                                                                                                                                                  | Externality                                                 | Contender                        | Wins                      |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- | -------------------------------- | ------------------------- |
| C1 Artifact | None. Largest non-stdlib, non-test program is `tools/almide-gates` (~2,000 lines), the project's own CI gate. Then a stdlib crawler, a Lisp interpreter, a raytracer                                                                  | —                                                           | —                                | —                         |
| C2 Method   | `research/grammar-lab/` — deciding syntax by measured LLM A/B rather than by taste                                                                                                                                                    | High                                                        | A language designer's intuition  | Yes                       |
| C2 Method   | The contract ledger: named `C-NNN` promises, bidirectionally linked to fixtures, CI-enforced, with a ratcheted counter that may only fall                                                                                             | High                                                        | Prose release notes              | Yes                       |
| C2 Method   | ADR with a mandatory falsifier, plus a rejection record with an operating rule                                                                                                                                                        | High                                                        | Git history as the record of why | Yes                       |
| C3 Proof    | Three Lean 4 belts: `almide-edit-belt` for the edit frame and modular typing over the kernel calculus, `almide-perceus-belt` for the RC discipline, `almide-race-belt`. No `sorry` in the 18 `.lean` files, grepped rather than built | High — theorems about a calculus, not about adopting Almide | —                                | —                         |
| C3 Proof    | The v1 trust spine: emit a certificate per build and re-verify it with an extracted checker (~1,400 lines of OCaml), collapsing the trusted base from the ~100k-line compiler                                                         | High                                                        | Testing the compiler             | Claimed, not yet complete |
| C4 Corpus   | 610 cross-target fixtures, 327 contracts, 426 `.almd` spec test files                                                                                                                                                                 | Medium                                                      | —                                | —                         |

C is thick in method and proof and empty in artifacts. That follows from B1: a
project that picked a measurable arbiter went and built measuring and verifying
equipment. It is not an effect of being young — the Lean belts and the syntax A/B
harness are in a repository whose first commit is 2026-03-07.

A6 limits C1 from the other side. There is no generation mechanism and no
serialization framework, so a foreign grammar or wire format has no way into an
Almide program except by hand. That means the only programs you can write with it
are its own parts. No amount of effort would have produced a Gale here.

`research/grammar-lab/REPORT.md` is the clearest example of the method. The short
lambda `(x) => expr` was tested against the then-current `fn(x) => expr` by
transpiling model output, across 10 tasks × 3 trials. The result was 86% against
86%, p = 1.0 on Haiku, and 100% against 100% on Sonnet. The `fn` lambda form was
removed on the strength of that.

## Where the thesis stands

`demo/make-verify/README.md` records the headline metric failing, and the
framing being rebuilt around the failure.

Measured directly, modification survival came out at roughly 100% in every
language, for a strong model on moderate tasks. That is a ceiling effect, so as
they put it, "that is the wrong thing to measure."

The replacement injects eight realistic modification mistakes and asks whether
the language makes each one visible at author or CI time. Almide caught 6 of 8 at
compile time; Python's `py_compile` caught 0. The MSR paper and its ablation
study are on hold in `docs/roadmap/on-hold/`.

All of this evidence is self-referential: the project's own benchmark, its own
metric, its own injected faults. Nothing is measured against an outside
yardstick.

## For Wado

Learned:

- A benchmark table earns belief by publishing the column that hurts. Their
  native scoreboard names the machine and the compiler version, checks that
  stdout is byte-identical across every variant before timing anything,
  interleaves the runs, and commits the raw per-run JSON. Then it reports n-body
  against two Rust baselines: 1.00× against same-shape Rust, and 0.73× against
  the array-based version. That second number says idiomatic Rust is a quarter
  faster, and they printed it anyway. One row carries a footnote explaining that
  it was re-measured and why the number moved. The wasm size table separates "as
  shipped" from "after `wasm-opt -Oz`", and calls the second one an opt-in that
  leaves the verified envelope.
- A proof is only worth as much as the sentence the project is willing to write
  about what it does not cover. Almide sells certificates, and then says an
  accepted one proves memory safety and nothing about whether the lowering picked
  the right semantics — a certified-sound function can still print the wrong
  string. The discipline is not the proof. It is publishing the gap, stage by
  stage, with the worst one named.
- A promise nobody can trace to something executable is just prose. The ledger's
  bidirectional gate is what prevents that: a specification heading no contract
  cites fails the build, and so does a contract citing no heading. Wado's golden
  fixtures hold the same kind of evidence but have no index, so there is no way
  to ask which promise a fixture locks down, or which promises nothing locks
  down at all.
- The cross-check is where a language's claims actually get tested, and Almide is
  the case that shows why. Three surface claims — no raw pointers, generics, no
  aliases — all hold in the documentation and all break in the standard library.
  No amount of reading the cheatsheet would have found that. Applied to Wado, the
  question is not whether the stdlib compiles. It is which claim the stdlib would
  have had to break in order to be written at all.
- Where the two surveys disagree is where the real design space is. Almide keeps
  a rejected design forever, because the reason is the asset. vibe deletes it,
  because a superseded entry becomes a false statement about the current build.
  Both positions are reasoned, and Wado's existing rule already sits with vibe.
  That makes it a choice to make, not a best practice to copy.

Take:

- [ ] A rejection record. Wado has none. The reasons for refusing lifetimes, a
      borrow checker, `unsafe`, macros, dynamic dispatch and ASI are written
      down nowhere, so an agent works them out again every time. The operating
      rule for the file matters as much as the list in it.

Refuse:

- An inventory of hidden operations. Checked against `spec.md`, and not needed.
  The operations that change what a program means are already specified there,
  each with its own section: bound-driven derivation of `Eq` / `Ord` / `Default`
  / `Serialize`, integer, float, sequence and collection literal coercion, and
  the automatic `&mut` to `&` coercion. Nothing of Almide's shape is left over,
  because Wado never inserts copies and then removes them — the ownership
  analysis picks the defensive copies once, at lowering. There is no elision
  pass, and no hand-written `.copy()` for one to defeat. The only thing actually
  missing was a statement that the copies are as-if, and that is now in
  [Memory Model](./spec.md#memory-model). How `&mut` is realized is an
  implementation detail and stays in its WEP.
- A falsifier field in the WEP template. A decision is allowed to rest on a
  preference.
- A contract ledger. Almide needs one because two backends implement the
  semantics separately. Wado has one target, and golden fixtures already cover
  the ground.

Hold:

- Keeping superseded alternatives in a WEP. `docs/CLAUDE.md` currently sends
  them to git history as the single source of truth. That works for a human
  reader and does not work for an agent, because an agent will not go looking.
  Changing the rule is cheap for new WEPs and expensive to apply to old ones.

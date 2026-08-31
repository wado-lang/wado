# Research: Language Survey — Almide

Survey against [the rubric](./research-language-survey.md).

7827 commits / first 2026-03-07 / surveyed at b900c57ed 2026-08-31
([almide/almide](https://github.com/almide/almide))

Arbiter: "Almide optimizes for minimal thinking tokens: the less an LLM has to
branch over syntax, semantics, repair strategies, or missing abstractions, the
faster, cheaper, and more reliable code generation becomes." — opens
`docs/design/DESIGN.md`.

A statically-typed language compiling to native (via Rust) and to wasm, holding
byte-identical observable output across the two as a hard invariant. Its stated
metric is modification survival rate: how often code still compiles and passes
its tests after a series of AI-driven edits.

## A. Surface

| Axis | Claim | Reality | Holds in stdlib | Unimplemented / Rejected |
| --- | --- | --- | --- | --- |
| A1 Canonicity | "One name per operation, no aliases" (`DESIGN.md`); the cheatsheet lists `string.length` as wrong | Prose convention only; nothing checks it | No — `stdlib/string_len.almd` defines `string.length` as an alias of `string.len`, and `list.len`/`list.length` both map to `almide_rt_list_len` | — |
| A2 Type vocabulary | Invented; primitives cut to `Int`, `Float`, `String`, `Bool`, `Unit`, `Path` | Sized integers exist as conversion modules (`int8`…`uint64`) reached by UFCS, not as surface types | — | Rejected: no external ABI to mirror |
| A3 Effects | `effect fn` marks I/O; a pure fn cannot call one (E006) | One bit. No effect names, no polymorphism, no handlers | Yes | Rejected — `REJECTED_PATTERNS.md` refuses algebraic effects, citing Gleam |
| A4 Errors | `Result[T, E]` only; `T!` is `Result[T, String]`, `T!E` is `Result[T, E]`, `T?` is `Option[T]` | Propagation is `!` and always explicit (ADR-0008 abolished auto-`?`); `!` never converts `E`, a mismatch stays a type error (ADR-0003) | Yes | Rejected: exceptions, `null`, implicit conversion at propagation |
| A5 Concurrency | `fan`, structured, no async/await | Deterministic by construction: `fan.race` picks the winner by least compute spent, ties by source order, same answer on every target and machine. `Compute` and `Duration` are separate types; a bare `Int` is not a time and there are no literal suffixes | Yes | Rejected: async/await (function colouring), goroutines, exposed `Future[T]` |
| A6 Code generation | No macros, no reflection | Generation lives in TOML definitions plus `build.rs` | — | Rejected |
| A7 Hidden operations | Zig's "no hidden control flow" cited, then deliberately departed from | `docs/design/HIDDEN_OPERATIONS.md` enumerates five, each with trigger condition, implementing file, and user impact: clone insertion, runtime embedding, Perceus RC insertion, `fan` threading, and one entry recording that auto-`?` was removed | — | — |

A4 carries a doctrine the other axes do not. `E = String` is the reporting
channel and the default; a variant `E` is the branching channel and earns its
cost only inside a closed domain; leaving that domain demotes it back to
`String` through a visible `map_err`, because no conversion hook exists. Two
lints hold it up: E035 warns on branching over an error's message text, E036
warns when a named `map_err` parameter drops `${e}`.

### The stdlib cross-check

232 of 309 stdlib files (75%) call `prim`, which the module header describes as
"the PRIMITIVE FLOOR: raw memory access … UNSAFE: addresses are unchecked".
Actual use is hand-written address arithmetic:

```almide
let x = prim.load64(sh + 12 + i * 8)
prim.store_str(dh + 12 + i * 8, y)
```

The public surface is 971 functions across 43 modules, declared generically with
empty bodies — `stdlib/*.almd` carries 808 `@intrinsic` annotations. The bodies
live in two places: `runtime/rs` (42 Rust files, 12,941 lines) for native, and
the self-hosted `.almd` files for wasm.

Generics reach the declaration but not the implementation. `List[A]` is one
generic signature over per-type-pair implementations hand-written as separate
files — `list_map_s2h` (scalar source, heap result), `list_fold_hsca`,
`list_to_string_ll`, `list_filter_str`, `map_skv`, `option_to_string_oi`. The
compiler dispatches between them, and `list_map_s2h`'s own comment records what
a wrong dispatch costs: `list.map_str` "would misread the i64 scalar slot as a
String handle and corrupt it". 3,955 function definitions back 971 public ones.

So three surface claims — no raw pointers, generics, no aliases — do not survive
contact with the language's own standard library.

## B. Design and governance

| Axis | Present | Scope |
| --- | --- | --- |
| B1 Arbiter | Yes | One sentence, opening the philosophy document; every other section derives from it |
| B2 Accept/reject criteria | Yes, numeric | Rejected if median MSR drops ≥ 3 points on any model, or if it introduces a second canonical form, or if it trades generation predictability for human aesthetics. Accepted if MSR measurably rises, or if it removes an error class that retry-with-diagnostic cannot repair |
| B3 The "why" axis | Yes | `docs/adr/`, 12 records. Its README states the problem directly: roadmap and spec both fail to preserve why, so the same argument is replayed and a rejected proposal returns with its rationale lost |
| B4 Falsifier | Yes, mandatory | Template field, with the rule "a decision whose retraction condition cannot be written is a preference, not a decision" |
| B5 Rejection record | Yes | `docs/design/REJECTED_PATTERNS.md`, 154 lines, with an operating rule: add on rejection with the reason, remove only on an edition-level change of direction, cite it when a PR proposes the feature. Carries a reversal in place — `??` was rejected, then reinstated by ADR-0005, and the rejection is kept as history |
| B6 Sync gate | Yes, partial | `check-readme-numbers.sh` refuses a bare number in the README, `gen-claims.sh` generates the public claims block from the contract ledger, `check-contracts.sh` fails on drift. Scope is numbers and contracts. Prose is unguarded, which is how A1 leaked |
| B7 Self-reported violations | Yes | `docs/specs/edit-locality.md` §3 lists eight standing violations of its own L1 invariant with mechanism and `file:line`, triaged into language bugs, backend obligations, and declared side conditions |
| B8 Who decides | No rule | The ADR rules constrain a decision's quality, not its author |

The governing invariant is worth naming because it is what the surface rules
serve. L1, the "edit frame": for a well-typed program and an edit to one
definition's body that preserves its signature, every execution that never
enters that definition has identical observable output. `edit-locality.md` §2
maps each language rule to the role it plays in enforcing L1 — mandatory return
types, no overloading, no glob imports, module-isolated inference, no
dynamically scoped handlers — and §5 makes it a gate: every change answers "does
this preserve L1" before landing.

## C. Spin-off value

| Kind | What | Externality | Contender | Wins |
| --- | --- | --- | --- | --- |
| C1 Artifact | None. Largest non-stdlib, non-test program is `tools/almide-gates` (~2,000 lines), the project's own CI gate. Then a stdlib crawler, a Lisp interpreter, a raytracer | — | — | — |
| C2 Method | `research/grammar-lab/` — deciding syntax by measured LLM A/B rather than by taste | High | A language designer's intuition | Yes |
| C2 Method | The contract ledger: named `C-NNN` promises, bidirectionally linked to fixtures, CI-enforced, with a ratcheted counter that may only fall | High | Prose release notes | Yes |
| C2 Method | ADR with a mandatory falsifier, plus a rejection record with an operating rule | High | Git history as the record of why | Yes |
| C3 Proof | Three Lean 4 belts, 0-sorry: `almide-edit-belt` proves the edit frame and modular typing for the kernel calculus, `almide-perceus-belt` the RC discipline, `almide-race-belt` | High — theorems about a calculus, not about adopting Almide | — | — |
| C3 Proof | The v1 trust spine: emit a certificate per build and re-verify it with an extracted checker (~1,400 lines of OCaml), collapsing the trusted base from the ~100k-line compiler | High | Testing the compiler | Claimed, not yet complete |
| C4 Corpus | 610 cross-target fixtures, 327 contracts, 426 `.almd` spec test files | Medium | — | — |

C is thick in method and proof and empty in artifacts, which follows from B1: a
project that named a measurable arbiter built measurement and verification
apparatus. This is not a maturity effect — the Lean belts and the syntax A/B
harness exist in a repository whose first commit is 2026-03-07.

`research/grammar-lab/REPORT.md` is the clearest instance. Short lambda
`(x) => expr` was tested against the then-current `fn(x) => expr` by transpiling
model output, over 10 tasks × 3 trials: 86% versus 86%, p = 1.0 on Haiku, 100%
versus 100% on Sonnet. The `fn` lambda form was removed on that result.

## Where the thesis stands

`demo/make-verify/README.md` records the metric failing and the framing being
rebuilt around it: measured directly, modification survival is roughly 100% in
every language for a strong model on moderate tasks — a ceiling effect, so "that
is the wrong thing to measure". The replacement injects eight realistic
modification mistakes and asks whether the language makes each visible at author
or CI time: Almide caught 6 of 8 at compile, Python's `py_compile` caught 0. The
MSR paper and its ablation study are on hold in `docs/roadmap/on-hold/`.

All of the evidence is self-referential — the project's own benchmark, its own
metric, its own injected faults. There is no measurement against an outside
yardstick.

## For Wado

Take, in order of cost to value:

- [ ] A rejection record. Wado has none; the reasons for refusing lifetimes, a
      borrow checker, `unsafe`, macros, dynamic dispatch, and ASI live nowhere,
      so an agent re-derives them each time. The operating rule matters as much
      as the list.
- [ ] An inventory of hidden operations. Wado's philosophy claims more here than
      Almide's — "the code you read is the code that runs" — while value-copy
      insertion and elision, bound-driven derivation of `Eq`/`Ord`/`Default`/
      `Serialize`, literal coercion, `&mut` to `&` coercion, reference
      write-back, and monomorphization are each documented only in their own WEP.
      There is no list. Publishing one is the principle being kept.
- [ ] A falsifier field in the WEP template. One line, and it composes with the
      existing rule that only the human decides.

Refuse:

- A contract ledger. Almide needs one because two backends implement the
  semantics separately; Wado has one target and golden fixtures already cover
  the ground.

Hold:

- Keeping superseded alternatives in a WEP. `docs/CLAUDE.md` currently sends
  them to git history as the single source of truth, which holds for a human
  reader and does not hold for an agent, since an agent will not go looking.
  Changing it is cheap for new WEPs and expensive to apply retroactively.

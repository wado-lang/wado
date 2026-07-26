# Gale — Action / Predicate Execution Design

Design notes for executing `{ ... }` actions and `{ ... }?` semantic predicates in generated parsers. Companion to [`TODO.md`](./TODO.md) and [`antlr4-compatibility.md`](./antlr4-compatibility.md), which track this work as the final compatibility stage. Design only — no implementation detail; progress is tracked in `TODO.md`.

## Requirements

1. Existing Java action bodies run under Wado. The descriptor corpus carries plain Java action bodies (extract-time template expansion baked them in), so this covers the corpus and the in-grammar Java of real-world grammars.
2. Actions can be written in Wado directly.

Java translation (java2wado) starts from "the Java subset that appears in ANTLR action bodies, written against the ANTLR runtime API" — expressions, statements, local declarations, attribute references — and grows on corpus demand toward general Java-to-Wado translation.

## What the corpus needs

- Descriptor corpus: small Java — prints, member arithmetic, lookahead tests, assignments to `returns` values. Attribute surface: `$ctx`, `$label.field`, `$TOKEN.text`, `$TOKEN.int`, `$text`, `$_p`, rule args / `returns` fields.
- Real-world grammars (RustLexer/Parser, TypeScriptLexer/Parser, ANTLRv4Lexer): every action is `{this.method()}` into a hand-written `superClass` base **outside the `.g4`**. Action translation alone runs none of them; the base class must exist in Wado (see SuperClass).

## Architecture

1. IR retention. Action / predicate source (text, span, position, language tag), rule signatures (args / `returns` / `locals`), prequels (`@init` / `@after`), and lexer-rule actions are retained in the IR instead of discarded.
2. Attribute resolution (language-independent). `$x`, `$x.text`, `$ctx`, `$_p`, `$text` are ANTLR semantics, not host-language semantics, so Gale resolves them itself before any host translation.
3. Translator. `translate(body, resolved attrs) -> Wado`. Two are shipped: identity (the body is already Wado — requirement 2) and java2wado (requirement 1). Both target the same runtime API.
4. Runtime layer. A small action-context API in the generated recognizer (lookahead, matched text, input access) that every translated body calls into. `superClass = Foo` becomes an effect interface the user implements and installs — this is how the real-world grammars run.

## IR retention

Retained per site: whether it is an action (`{ ... }`) or a predicate (`{ ... }?`), its verbatim host-language body, and its position within the alternative (how many significant elements precede it). Rules additionally retain their `args` / `returns` / `locals` signatures, `@init` / `@after` prequels, and `throws` / `catch` / `finally` (retained for completeness; `catch` / `finally` are not executed — Wado has no exceptions, and mapping ANTLR's recovery hooks onto Gale's resilient-parse model is a separate decision). The grammar retains its action language (`options { language = X }`, default Java).

Actions are recorded in a per-alternative sidecar, **not** as a new element in the matched sequence. This keeps the matched-element sequence — and every index-based analysis over it (LR self-reference, suffix walks, prediction, scan) — exactly as before, so retention alone is byte-identical: a generated recognizer never sees an action until a later phase reads the sidecar.

## Attribute resolution

A language-independent scanner (skipping strings and comments) finds `$ident(.ident)?` references in a body. Resolution is per rule, against an environment of: labels (`x=e`, `x+=e`; list labels resolve to list slots), rule args / `returns` / `locals`, unlabeled element references by name, and the specials `$text` / `$ctx` / `$start` / `$stop` / `$_p` (parser) and `$text` / `$type` / `$channel` / `$mode` (lexer). Member access on a resolved token (`.text`, `.type`, `.line`, `.pos`, `.index`, `.channel`, `.int`) and on a resolved rule (its `returns` fields plus `.text` / `.ctx` / `.start` / `.stop`) is validated. A shared substitution engine rewrites each reference to a Wado expression over the value channel / context API; the translator only translates the host code around them. An unresolvable reference is a loud generation error, never a silent passthrough.

## Value channel — args / returns / locals

A rule declaring any of args / `returns` / `locals` gets a generated value struct, threaded through its parse function and returned to callers. Fields are default-initialized; rule args become extra parameters. A caller binds the returned value struct to a local when an action references it, so `$a.v` resolves to that local. Rules with no declarations keep a unit return, so actionless grammars stay byte-identical, and scan functions are unaffected (no values or actions during scan). Left-recursive rules keep the left operand's values in a local across the precedence-climbing loop; the corpus's `$ctx`-cast LR-binary idiom maps onto the same locals. General `$ctx`-mediated typed child access needs a typed CST and is a later item.

## Runtime context API

Actions and predicates call a small API; both Wado-written and translated bodies target it. Parser side:

| API                    | Backs                                                           |
| ---------------------- | --------------------------------------------------------------- |
| `p.la(k)` / `p.lt(k)`  | `_input.LA(k)` / `_input.LT(k)` (token kind / index)            |
| `p.token_text(i)`      | `.getText()` on a token                                         |
| `p.rule_text()`        | `$text` — input consumed by the current rule so far             |
| `p.input_text()`       | `_input.getText()` — the whole input                            |
| `p.expected_names()`   | `getExpectedTokens().toString(getVocabulary())`                 |
| `p.rule_string_tree()` | `$ctx.toStringTree(this)` — renders the node under construction |
| `p.emit(s)`            | action prints (see Effects and printing)                        |

Lexer side: matched text, column, set-type / set-channel / skip / more, and mode push / pop / set.

Harness knobs from the corpus map to the nearest Gale equivalent or a documented no-op: parse-tree construction is always on; exact-ambiguity / DFA-dump knobs are no-ops (diagnostics differ); the bail strategy maps to a one-error cap.

## Translator

Translation runs at generation time — deterministic and pure. A failure is a generation diagnostic carrying the fragment's span, never a silent no-op. Actions execute for grammars whose action language Gale can emit: `language = Wado` via the identity translator, `language = Java` via java2wado.

- Identity translator (`language = Wado`): body and types pass through; only `$`-references are substituted.
- java2wado (`language = Java`): a real (small) Java parser — not regex rewriting, since string concatenation, operator precedence, and `this.`-rewriting can't be done soundly by span substitution — that re-emits the same Java subset onto the same runtime API, filling `$`-references through the shared resolver. It maps `System.out.println/print` to `p.emit`, the ANTLR recognizer accessors (`getText` / `_input.LA` / `getCharPositionInLine` / lexer command methods) to the context API, and the Java scalar types to their Wado equivalents. Method calls with no semantic redirect are case-converted to snake_case. Anything outside the subset is a loud diagnostic; the subset grows on corpus demand.

Members (`@members` / `@parser::members` / `@lexer::members`): field declarations become fields on the generated recognizer (with translated initializers), method declarations become methods. A Java `@header` (import statements) has no Wado meaning — warn and drop; a Wado `@header` is module-level Wado and passes through.

The Java rollout is cover-then-flip: a Java grammar's actions were discarded (byte-identical to actionless), the corpus subset is covered, then Java is flipped emittable in one step. The one principled carve-out is `superClass` grammars, whose actions call a hand-written base outside the `.g4` — handled by the SuperClass mechanism below, not treated as a java2wado coverage gap.

## SuperClass — an effect interface

ANTLR's `superClass` is object inheritance: the generated recognizer extends `Foo`, so `{this.m()}` calls a base method that reaches the recognizer's own runtime (`_input.LA`, `getText`, `getCharPositionInLine`) through the shared `this`, and base state (e.g. a lexer's brace / template stacks) rides on the same object. Wado has no inheritance, no `dyn`, value semantics, and a reference write-back model — so the two things inheritance fuses, base _state_ and recognizer _runtime_, must become two objects, connected without threading a handle through every generated function.

The Wado-native isomorphism is the **effect system**, not a trait. superClass needs two capabilities: (A) an indirect call into user-provided behavior, and (B) ambient reach to that behavior from deep inside prediction / scan without a threaded parameter. `dyn` supplies only (A); an effect supplies both — the effect's dispatch is itself the ambient reach (B). A threaded trait (generic or `dyn`) would force (B) back into a cross-cutting parameter on every generated function and monomorphize the whole recognizer per base type. The effect keeps the hot scan path byte-identical to a non-superClass grammar and pays one indirect call only where a predicate / action fires. Because base state (the handler's `self`) and the recognizer runtime (passed as an operation argument) are separate objects, the self-aliasing a base-as-field model would hit never arises.

`options { superClass = Foo }` generates an effect interface `Foo` in the recognizer module, with one operation per distinct `this.method` call site. A predicate call becomes an operation returning `bool`; an action call becomes a unit-returning operation. Each operation receives a `LexerView` — a match-window handle over the char stream plus the token start and the live match cursor, exposing the runtime a base method reaches through `this`: lookahead, matched text, and the cursor. (The lexer match cursor is a match-time local, not a field of the generated lexer, so the runtime arrives as a view built at the call site rather than a bare recognizer reference.) The `&self` / `&mut self` split is the handler's choice. Every corpus and real-world call is zero-argument beyond that view; a call with extra arguments is a loud error pending an explicit signature source.

The call site lowers to an ambient operation call — no handler is threaded. The public entries carry the effect in their signature (`tokenize`, `parse`, and the per-rule entries; a combined grammar's parser entries propagate the lexer's base effect because they tokenize). Safety then falls out for free: a caller that never installs a handler fails to compile with a missing-effect error — the "omitting an impl is a compile error, not a silently predicate-free parser" guarantee, enforced by the effect checker rather than a bespoke check. No generic entry is emitted; the user installs a concrete handler at the call site:

```wado
struct TsBase { brace_depth: i32 }        // base state — a plain user struct
impl TypeScriptLexerBase for TsBase {
    fn is_regex_possible(&self, view: LexerView) -> bool { resume self.brace_depth == 0 && view.la(1) != ('/' as i32) }
    fn process_open_brace(&mut self, view: LexerView) { self.brace_depth += 1; resume () }
}

let mut base = TsBase { brace_depth: 0 };
with TypeScriptLexerBase => &mut base do { let toks = tokenize(&input); ... }
```

Cost model (verified on a probe): a handler operation with no post-resume code hits the `resume`→`return` optimization and lowers to a single indirect call through the effect's dispatch — no Wasm stack switching / JSPI. The dominant per-character scan path emits no effect call and stays byte-identical to a non-superClass grammar; only the rare predicate / action sites pay the indirect call.

Gating: a non-superClass grammar emits no interface, no effect clause, and no operation call — byte-identical output.

ATN purity holds: a superClass predicate is pre-evaluated at the decision (in the caller, where the handler is ambient) via the existing exclusion path, so the simulator body stays a pure function of the token stream.

Split grammars: a lexer and its sibling parser each declare their own `superClass` (lexer base vs parser base). Gale wires the lexer's base; the merge keeps the lexer / combined split's `superClass` so the parser's does not clobber it.

Lifecycle overrides: a base may not only add helpers but _override_ recognizer methods (`nextToken` / `emit` / `reset`, e.g. to track the last token). These map to fixed hook operations the recognizer calls at the right point; the exact hook set is enumerated on real-grammar demand, not designed up front.

`tokenVocab` falls out separately and independently of the effect machinery: another grammar's generated token constants are imported by name.

## Predicates in prediction

Each predicate compiles to a standalone effect-free function callable from every decision path, classified as context-independent (no `$arg` / `$local` / `$ret`) or context-dependent.

1. Static dispatch: an alt-initial predicate guards its branch. Only alt-initial predicates participate in prediction (ANTLR's "visible" predicates); a mid-alt predicate evaluating false is a parse failure at that point, feeding normal recovery.
2. Scan tournament: a context-independent predicate is evaluated before scanning its gated alternative (false excludes it). A context-dependent predicate is treated as true during cross-rule scans — matching ANTLR, which ignores dependent predicates evaluated outside their owning context — and evaluated for real in the owning rule's dispatch.
3. ATN simulator: because prediction predicates are alt-initial, the caller pre-evaluates each gated alternative at the decision and excludes the false ones from the simulator's seed, so the simulator stays a pure function of the token stream.

All predicates false at a decision produces a "no viable alternative" diagnostic. Purity is enforced by the type system (stronger than ANTLR's convention); ambient logging remains possible, which is what the corpus's printing predicates need. Predicate evaluation counts and ordering need not match ANTLR (different algorithms) — the chosen parse must match, the trace need not.

## Action execution timing

- Actions are emitted on the parse side only; scans stay side-effect-free.
- Speculation: Gale's hybrid dispatch has save-and-rewind paths. An action must not observably execute inside an attempt that rewinds, so it is suppressed while speculating.
- `@init` runs at rule entry before the first decision; `@after` runs after the body, before the node is finished (so a rule-tree render sees the complete children).
- Left recursion: an alternative's actions move with the alternative into the precedence-climbing loop; `$_p` resolves to the current precedence threshold.

## Effects and printing

Actions print through `p.emit`, appended to a buffer on the parse result and mirrored to stdout under a generator option. This keeps generated parse signatures effect-free (no effect plumbing through the recursive descent), makes descriptor output comparison a plain string equality, and still gives CLI users real stdout. Wado-written actions may also use ambient logging directly. Effect-generic parse functions (user actions with arbitrary effects via handlers) are a future extension nothing here blocks.

## Error recovery and values

- An action executes iff its op path is reached normally; ops skipped by recovery skip their actions.
- Value structs are default-initialized, so a value of a missing / errored sub-rule reads the default, never traps.
- Predicates still evaluate during recovery dispatch (they gate which alternative recovery resumes into).
- Diagnostics and tree shape are unchanged from the resilient behavior.

## Lexer semantics

- Element actions run when the rule wins the longest match, in element order, with the cursor state they were passed; they never run for losing candidates. The winner-replay re-runs the rule's own match to place them, re-selecting each alternation match-only first so a losing arm stays silent.
- Predicates evaluate mid-match, position-sensitively, in both the single-pass emitter and the ATN-class lexer path; a false predicate rejects the candidate.
- Lexer commands stay typed IR (`-> skip`, etc.); set-type / set-channel from actions compose with them, action last-wins.
- The lexer is a single context (mirroring the parser): it carries the input, member state, the action-effect latches, and the print sink, so actions and predicates reach everything through one handle.
- Print output is a property of the whole tokenization, not of one token: the sink accumulates across the run and `tokenize` hands it to the token stream, so `to_lexer_string` renders it ahead of the dump and `parse` seeds the parser's sink with it — lexer prints precede parser prints, as they do on ANTLR4's stdout.

## Acceptance

- `SemPredEvalParser` / `SemPredEvalLexer` descriptors become the predicate suite.
- Descriptors whose output is action prints become comparable: run the generated parser, compare against the descriptor output.
- Composite descriptors' output comparison unblocks.
- Real-world grammars: driver tests with hand-written Wado SuperClass handlers (Rust `>>` splitting, TypeScript regex / division) as fixtures.
- The published jar stays the black-box oracle for order / count questions (license hygiene).

## Status

Design phases, tracked at the capability level (see `TODO.md` for the working task list):

- IR retention — done, byte-identical.
- Attribute resolution, value channel, and Wado (identity-translator) actions, including `@init` / `@after`, print-style actions, and the parser runtime-context API — largely done; the general `$ctx` typed-child access remains.
- Predicates in prediction (static dispatch, scan tournament, ATN exclusion; parser and single-pass lexer) — largely done; the ATN-class lexer path and the remaining lexer `$`-attribute surface remain.
- java2wado for the corpus parser subset plus members translation — done, lexer bodies included: codegen stages every Java lexer action / predicate into Wado before emit, so the lexer emitters stay language-agnostic (the same staging the superClass call rewrite uses).
- Lexer actions (winner-replay: set-type / channel / skip / more / mode ops, single- and multi-alt), the lexer print sink, and action placement (mid-element and nested-group, via a match replay that drops each action at its cursor) — done; actions under a `Repeat` and the ATN-class lexer path remain.
- SuperClass effect interface — done for predicate-only lexer bases (RustLexer runs, tokenize and parse, through a hand-written handler); action-op bases, parser-rule superClass predicates, and lifecycle-override hooks remain.

## Open questions

- Where Wado actions live: in-grammar (`language = Wado`) and the SuperClass effect handler are primary. A sidecar id→snippet mapping is fragile — keep as an escape hatch only?
- SuperClass operations with arguments: the signature source (sidecar vs a user-pre-declared interface).
- SuperClass lifecycle-override hook set: which recognizer methods a base may override become fixed effect operations — enumerated on real-grammar demand.
- Whether a Kiln generator option may override the action language.
- Value semantics after recovery beyond default-init: is the default always right for user types?
- Predicate eval-count divergence policy: how much trace mismatch is acceptable before a descriptor is skipped rather than marked todo.
- IR details: which element a precedence option legally attaches to (confirm via the jar); whether upstream accepts `@init` / `@after` on lexer rules.
- `catch` / `finally` execution under Gale's resilient-parse model (no exceptions in Wado) — parked; the IR retains them.
- Effect-generic parse functions (user actions with real effects via handlers) — future extension.
- Java numeric promotion: java2wado does not model Java's implicit numeric widening, so mixing an `i32` token member with a wider value-channel field mismatches Wado's strict widths. No corpus grammar hits this, and the failure is a loud type error, not a silent miscompile; a proper fix threads Java's promotion rules through the emitter.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only.

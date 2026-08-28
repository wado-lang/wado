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

A language-independent scanner (skipping strings and comments) finds `$ident(.ident)?` references in a body. Resolution is per rule, against an environment of: labels (`x=e`, `x+=e`; list labels resolve to list slots), rule args / `returns` / `locals`, unlabeled element references by name, and the specials `$text` / `$ctx` / `$start` / `$stop` / `$_p` (parser) and `$text` / `$index` / `$pos` / `$type` / `$channel` / `$mode` (lexer, the last three being the commit latches an action assigns — a predicate, evaluated for losing candidates too, settles nothing and is refused). A reference is resolved with how its site uses it: the match-window reads are not assignable, and `$mode`'s read and write are different fields (see Lexer semantics). Member access on a resolved token (`.text`, `.type`, `.line`, `.pos`, `.index`, `.channel`, `.int`) and on a resolved rule (its `returns` fields plus `.text` / `.ctx` / `.start` / `.stop`) is validated. A shared substitution engine rewrites each reference to a Wado expression over the value channel / context API; the translator only translates the host code around them. An unresolvable reference is a loud generation error, never a silent passthrough.

## Value channel — args / returns / locals

A rule declaring any of args / `returns` / `locals` gets a generated value struct, threaded through its parse function and returned to callers. Fields are default-initialized; rule args become extra parameters. A caller binds the returned value struct to a local when an action references it, so `$a.v` resolves to that local. Rules with no declarations keep a unit return, so actionless grammars stay byte-identical, and scan functions are unaffected (no values or actions during scan). Left-recursive rules keep the left operand's values in a local across the precedence-climbing loop; the corpus's `$ctx`-cast LR-binary idiom maps onto the same locals. General `$ctx`-mediated typed child access needs a typed CST and is a later item.

A cast `$ctx` member reaching a name the enclosing alternative already binds needs no typed CST: `((XContext)$ctx).INC()` names the alternative's own `INC` token, so it resolves to whatever `$INC` resolves to there, and `!= null` on it becomes the token-presence test. A name the alternative does not bind stays a loud error rather than a guess.

### The value channel is the context object

What ANTLR keeps on a context object, Gale keeps in the value struct: both live exactly as long as one rule invocation. So a capture written at one element and read by an action somewhere else in the invocation — a labeled rule call's token range (`$x.text`), the builder row its node opens at (`$x.ctx`) — is a field of `<Rule>Vals`, written where the element matches and read wherever an action sits.

The rule's own start token (`$text` / `$start` / `$stop`) needs no field: `_rule_start_tok` is bound at every shape's body entry, so it is already in scope at every action in that body. What it does need is that the walk deciding to bind it sees the same bodies the translator substitutes for — the rule's prequels, its alternatives, and the groups nested in them.

This is what makes the reads well-scoped by construction. A rule's body is not one block: an alternative is a block, a repeat body is a block, and a multi-alt or left-recursive rule spreads its alternatives across `_alt_<n>` / `_atom` / `_lr_<n>` functions. A capture held in a local is visible only inside the block that declares it, so a prequel (`@init` before the body, `@after` after it), an action in a sibling branch, or anything in another function reads a local that is not there — a generated module that does not compile, or a silently empty value. A field has none of those cases: the struct is threaded into every helper and returned from it (each mutates its own copy and hands it back), so a write anywhere in the invocation is visible to every later read in it.

The channel therefore opens for a rule that captures anything, not only one declaring args / `returns` / `locals`. Grammars whose actions are off stay byte-identical: the capture, the field, and the struct are all gated on action emission.

A repeat writes its capture once per iteration, so a reference reads the last match — ANTLR's `$x` for a looped label.

## Runtime context API

Actions and predicates call a small API; both Wado-written and translated bodies target it. Parser side:

| API                    | Backs                                                           |
| ---------------------- | --------------------------------------------------------------- |
| `p.la(k)` / `p.lt(k)`  | `_input.LA(k)` / `_input.LT(k)` (token kind / index)            |
| `p.token_text(i)`      | `.getText()` on a token                                         |
| `p.rule_text()`        | `$text` — input consumed by the current rule so far             |
| `p.input_text()`       | `_input.getText()` — the whole input                            |
| `p.rule_stack()`       | `getRuleInvocationStack()` — the open nodes, innermost first    |
| `p.rule_string_tree()` | `$ctx.toStringTree(this)` — renders the node under construction |
| `p.rule_tree_at(row)`  | `$x.ctx` — renders one labeled rule call's own node             |
| `p.emit(s)`            | action prints (see Effects and printing)                        |

Two ANTLR reads have no API row, because Gale does not keep that state the way ANTLR does:

- The rule-invocation stack is already in the tree builder — the nodes open at the action's site, rendered innermost first as rule names. No new recognizer state.
- The expected-token set is a generation-time property, not a runtime query: `getExpectedTokens().toString(getVocabulary())` folds to the rendering codegen computed, through the vocabulary names ANTLR's `Vocabulary` gives (a token's literal when it has one, else its symbolic name). Codegen answers at a rule prequel, where the set is the rule's FIRST (plus FOLLOW when the rule is nullable) at entry and its FOLLOW at exit; anywhere else, and for a rule whose FOLLOW fixed point is not exact, the body is reported rather than given a set that is not ANTLR's.

`PredictionMode` and `dumpDFA` describe ANTLR's simulator rather than the grammar, so they are out of scope permanently: a descriptor that prints one is not a Stage C gap.

Lexer side: matched text, column, set-type / set-channel / skip / more, and mode push / pop / set.

Harness knobs from the corpus map to the nearest Gale equivalent or a documented no-op: parse-tree construction is always on; exact-ambiguity / DFA-dump knobs are no-ops (diagnostics differ); the bail strategy maps to a one-error cap.

## Translator

Translation runs at generation time — deterministic and pure. A failure is a generation diagnostic carrying the fragment's span, never a silent no-op. Actions execute for grammars whose action language Gale can emit: `language = Wado` via the identity translator, `language = Java` via java2wado.

- Identity translator (`language = Wado`): body and types pass through; only `$`-references are substituted. It reads no host code, so a mistake in one — a name that is not there, a `&mut self` member called where the handle is `&Lexer` — is a compile error on the generated module at its own call site rather than a Gale diagnostic. That is the contract, not a gap: the body is already Wado, and the Wado compiler is the thing that reads Wado. The asymmetry with java2wado, which does refuse both, is the point rather than an oversight — java2wado has to resolve names because it is re-emitting a foreign language, and reporting the same things here would mean name resolution, types and receivers for Wado inside the generator: a second Wado front end, maintained against the first. The failure it would move is already loud and already points at the offending line. Do not re-open this as a defect; a text scan standing in for that front end is the unsound version and is rejected outright.
- java2wado (`language = Java`): a real (small) Java parser — not regex rewriting, since string concatenation, operator precedence, and `this.`-rewriting can't be done soundly by span substitution — that re-emits the same Java subset onto the same runtime API, filling `$`-references through the shared resolver. It maps `System.out.println/print` to `p.emit`, the ANTLR recognizer accessors (`getText` / `_input.LA` / `getCharPositionInLine` / lexer command methods) to the context API, and the Java scalar types to their Wado equivalents. Method calls with no semantic redirect are case-converted to snake_case. Anything outside the subset is a loud diagnostic; the subset grows on corpus demand.

Every spelling of a name resolves in one place. Java reaches a declaration bare, through `this.`, or as a call, and ANTLR adds `$name` for a rule argument — four spellings of one thing. Each used to decide for itself what it had found, so a rule about a name (which receiver it emits under, whether it has to be declared, whether a lexer predicate may reach it, whether it is the value channel's storage rather than a Wado local) held at some spellings and not at others, and each fix added a spelling for the next one to miss. `resolve_name` answers what a name denotes — a body local or parameter, a rule argument, a `@members` field, a `@members` method, or nothing the recognizer declares — and every emit site consults it, so each rule is stated once and a spelling cannot drift from its siblings.

Members (`@members` / `@parser::members` / `@lexer::members`): field declarations become fields on the generated recognizer (with translated initializers), method declarations become methods. A Java declaration is snake_cased — field as well as method — because every read of one goes through the same case conversion, so `this.nestLevel` and the bare `nestLevel` have to land on the one field the struct carries. Java reaches a member bare, so an unqualified name is resolved against the declarations before it is passed through — in a parser body as in a lexer one — with the body's own locals and parameters shadowing them, as Java's scoping does. A Java `@header` (import statements) has no Wado meaning — warn and drop; a Wado `@header` is module-level Wado and passes through.

ANTLR concatenates a recognizer's `@members` blocks, and Gale translates them as one body: a method in the second block calls one declared in the first, and reads a field declared in either. Translating a block in isolation would refuse those as undeclared and would compute the receiver split below over a partial call graph.

A lexer member's receiver follows from its body: a method that writes no field, prints nothing, and calls no mutating member takes `&self`, the rest `&mut self` — a fixed point over the whole of the recognizer's members, so a reader stays `&self` however deep its call chain. That split is what decides which members a predicate may reach — a predicate runs inside `try_<rule>(lx: &Lexer, ...)`, where the tournament must not mutate through a losing candidate, so a predicate reaches only the `&self` half and an action either. A Java predicate calling a mutating member is refused by name, in either spelling (`this.m()` and the bare `m()` are the same call). A Wado member writes its own receiver and is read as written; a Java one is derived.

The Java rollout is cover-then-flip: a Java grammar's actions were discarded (byte-identical to actionless), the corpus subset is covered, then Java is flipped emittable in one step. The one principled carve-out is `superClass` grammars, whose actions call a hand-written base outside the `.g4` — handled by the SuperClass mechanism below, not treated as a java2wado coverage gap.

## SuperClass — an effect interface

ANTLR's `superClass` is object inheritance: the generated recognizer extends `Foo`, so `{this.m()}` calls a base method that reaches the recognizer's own runtime (`_input.LA`, `getText`, `getCharPositionInLine`) through the shared `this`, and base state (e.g. a lexer's brace / template stacks) rides on the same object. Wado has no inheritance, no `dyn`, value semantics, and a reference write-back model — so the two things inheritance fuses, base _state_ and recognizer _runtime_, must become two objects, connected without threading a handle through every generated function.

The Wado-native isomorphism is the **effect system**, not a trait. superClass needs two capabilities: (A) an indirect call into user-provided behavior, and (B) ambient reach to that behavior from deep inside prediction / scan without a threaded parameter. `dyn` supplies only (A); an effect supplies both — the effect's dispatch is itself the ambient reach (B). A threaded trait (generic or `dyn`) would force (B) back into a cross-cutting parameter on every generated function and monomorphize the whole recognizer per base type. The effect keeps the hot scan path byte-identical to a non-superClass grammar and pays one indirect call only where a predicate / action fires. Because base state (the handler's `self`) and the recognizer runtime (passed as an operation argument) are separate objects, the self-aliasing a base-as-field model would hit never arises.

Which recognizer a `superClass` names follows ANTLR (`vendor/antlr4/doc/options.md`): a `lexer grammar` gives the lexer's base, a `parser` or combined grammar the parser's. A lexer and its sibling parser therefore declare two independent bases, and the merged `Grammar` carries one slot each — a single `options` list cannot express both. A body naming `this` with no base wired for its own recognizer has no receiver at all, so it is reported and dropped rather than emitted.

`options { superClass = Foo }` generates an effect interface `Foo` in the recognizer module, with one operation per distinct `this.method` call site. A predicate call becomes an operation returning `bool`, evaluated in the match fn; an action call becomes a unit-returning operation, run from the winner replay like any other lexer action, so a losing candidate never reaches the base. Because the call rewrite turns `{this.m();}` into Wado on its own, an action body that is nothing but base calls runs even for an action language Gale does not translate (`language = Java` under a `superClass`, which java2wado carves out) — a body carrying host code around the call needs that translator, so it is reported instead. Each operation receives a `LexerView` — a match-window handle over the char stream plus the token start and the live match cursor, exposing the runtime a base method reaches through `this`: lookahead, matched text, and the cursor. (The lexer match cursor is a match-time local, not a field of the generated lexer, so the runtime arrives as a view built at the call site rather than a bare recognizer reference.) An action op additionally receives the generated `Lexer` as `&mut` — ANTLR's command surface: `set_type` / `set_channel` / `skip` / `more` / `push_mode` / `pop_mode` / `set_mode`, plus the `mode_depth()` a base branches on. Each mode command is a single latch the commit applies once per token, so issuing two from one action asserts rather than silently reordering them; there is no current-mode read, since `pop_mode` could not restore the mode beneath the stack top. A predicate stays on the read-only view, since it is evaluated for losing candidates too. The `&self` / `&mut self` split is the handler's choice. An operation that takes arguments gets its signature from its call sites: every site must agree on arity, and each position must be a literal whose Wado type is evident (`this.p("of")` → one `String`). Sites that disagree, or an argument that is not a literal, leave the base unwired with a diagnostic naming the operation — the rewrite would otherwise have to invent a type. This is the whole of what `TypeScriptParser`'s `{this.p("of")}?` needs; a richer signature source (a pre-declared interface) waits for a grammar that needs one. A name called from both a predicate and an action body is refused the same way — an operation is one or the other, and the two call forms differ.

The call site lowers to an ambient operation call — no handler is threaded. The public entries carry the effect in their signature (`tokenize`, `parse`, and the per-rule entries; a combined grammar's parser entries propagate the lexer's base effect because they tokenize). Safety then falls out for free: a caller that never installs a handler fails to compile with a missing-effect error — the "omitting an impl is a compile error, not a silently predicate-free parser" guarantee, enforced by the effect checker rather than a bespoke check. No generic entry is emitted; the user installs a concrete handler at the call site:

```wado
struct TsBase { brace_depth: i32 }        // base state — a plain user struct
impl TypeScriptLexerBase for TsBase {
    fn is_regex_possible(&self, view: LexerView) -> bool { resume self.brace_depth == 0 && view.la(1) != ('/' as i32) }
    fn process_open_brace(&mut self, view: LexerView, lx: &mut Lexer) { self.brace_depth += 1; resume () }
}

let mut base = TsBase { brace_depth: 0 };
with TypeScriptLexerBase => &mut base do { let toks = tokenize(&input); ... }
```

Cost model (verified on a probe): a handler operation with no post-resume code hits the `resume`→`return` optimization and lowers to a single indirect call through the effect's dispatch — no Wasm stack switching / JSPI. The dominant per-character scan path emits no effect call and stays byte-identical to a non-superClass grammar; the predicate / action sites pay the indirect call where they fire, and the `emit` hook pays one per emitted token. A grammar with no `superClass` emits neither the interface nor the hook call.

Gating: a non-superClass grammar emits no interface, no effect clause, and no operation call — byte-identical output.

ATN purity holds: a superClass predicate is pre-evaluated at the decision (in the caller, where the handler is ambient) via the existing exclusion path, so the simulator body stays a pure function of the token stream.

Parser side: a parser base's operations are predicates taking `p: &Parser`. There is no view here — the parser's cursor is a field, so the recognizer itself is what an ANTLR base method reads through `this`, and the action-context API (`la` / `lt` / `token_text` / `token_start` / `rule_text` / …) becomes that base's public surface. There is no command surface either, so a name reached from an action body is refused the way an argument-passing one is. Every generated parse function carries the base effect — a predicate can sit in any rule and an effect propagates to each caller — while scan functions stay exempt, evaluating no action.

Lifecycle overrides: a base may not only add helpers but _override_ recognizer methods. These are fixed operations on every generated interface, each with a default body, so a base opts in by implementing one exactly as an `@Override` does and every other base is untouched. Real-grammar demand has enumerated one so far: `emit(view, ty, channel) -> i32`, called for each token the lexer commits — not a skipped or `more`d match, and not the EOF sentinel — returning the type to emit. ANTLRv4's `LexerAdaptor` retypes `ID` to `TOKEN_REF` / `RULE_REF` there and tracks the current rule type it branches on; TypeScript's records the last default-channel token for `IsRegexPossible`.

`tokenVocab` falls out separately and independently of the effect machinery: another grammar's generated token constants are imported by name.

## Predicates in prediction

Each predicate compiles to a standalone effect-free function callable from every decision path, classified as context-independent (no `$arg` / `$local` / `$ret`) or context-dependent.

1. Static dispatch: an alt-initial predicate guards its branch. Only alt-initial predicates participate in prediction (ANTLR's "visible" predicates); a mid-alt predicate evaluating false is a parse failure at that point, feeding normal recovery.
2. Scan tournament: a context-independent predicate is evaluated before scanning its gated alternative (false excludes it). A context-dependent predicate is treated as true during cross-rule scans — matching ANTLR, which ignores dependent predicates evaluated outside their owning context — and evaluated for real in the owning rule's dispatch.
3. ATN simulator: because prediction predicates are alt-initial, the caller pre-evaluates each gated alternative at the decision and excludes the false ones from the simulator's seed, so the simulator stays a pure function of the token stream.

All predicates false at a decision produces a "no viable alternative" diagnostic. Purity is enforced by the type system (stronger than ANTLR's convention); ambient logging remains possible, which is what the corpus's printing predicates need. Predicate evaluation counts and ordering need not match ANTLR (different algorithms) — the chosen parse must match, the trace need not.

A group's alternatives are a decision like a rule's, so the three paths above are the same three inside a group: the alternative's gate condition joins the group's kind check, the scan tournament excludes a gated alternative before scanning it, and a group decision that escalates seeds the simulator with the false ones disabled. Nothing about a group makes its predicates a different kind of predicate — `('s' | 'x' | {pred}? NL)+` decides per iteration exactly as `s : 's' | 'x' | {pred}? NL ;` decides per call.

That includes the report: a **required** group whose dispatch takes no alternative — a token outside every first set, or every alt-initial predicate false — reports no viable alternative, as a rule dispatch does. A `+`'s mandatory first iteration is such a position, since the loop guard covers only the iterations after it. Silence is for a group the caller may legitimately skip: an optional, or a loop iteration that guard has already ruled viable.

## Actions inside a group

An action belongs to the alternative it is written in, whether that alternative is a rule's or a group's, and runs when the alternative is taken. One element walker therefore serves every body — rule alternative, group alternative, LR suffix, a transparent group's inlined elements — taking the alternative's actions and its own op bindings, so an action's position and the labels it reads come from the body it sits in rather than the one that encloses it. Under a `Repeat` this runs the action once per iteration, which is what a group under `*` means.

Where a body is emitted as several shape branches (an optional whose absent positions are skipped), only the elements are shape-conditional: an action between them is written in the alternative, not in the optional, so it runs in every branch. An action nested inside the skipped element belongs to that element's own body and goes with it.

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
- A repeat replays iteration by iteration: each one is probed match-only first, and only an iteration that matches is walked again with its actions on. An iteration the loop tries and rolls back therefore leaves no trace — the same guarantee the winner replay gives a losing alternation arm. A repeat the non-greedy or lookahead-aware emitter restructures the sequence around keeps that guarantee through its own two passes: the scan that settles where the loop stops runs match-only, and a second walk re-runs what it kept — the iterations up to the accept, then the rest of the alternative, whose actions travel with it so one written after the repeat keeps its cursor.
- Predicates evaluate mid-match, position-sensitively; a false predicate rejects the candidate. The single-pass emitter does this at every position, including inside a repeat. The ATN-class path does not: the lexer ATN has no predicate transition. A predicate the lexer will not evaluate — one in an ATN-class rule, or one the translator cannot emit — is reported as `UnsupportedAction` naming the rule, so the rule matching without it is never silent. Whether that report should also stop generation is open; today it does not, and the two paths answer alike.
- A body the translator cannot emit is a diagnostic, never a generator panic: codegen stages every lexer action and predicate into Wado before emit — the same staging the Java rewrite and the superClass call rewrite already use — so the emitters splice ready text and every failure has a `ctx` to report to.
- Lexer commands stay typed IR (`-> skip`, etc.); set-type / set-channel from actions compose with them, action last-wins. The type and channel latches are seeded from the tournament's own answer before the replay, so an action that writes neither hands it back unchanged and one that reads `$type` sees the token's current type rather than "nothing has written this".
- `$mode` is two things, so it is two fields. Its write is a command the commit applies once per token, `-1` until an action issues one; its read is the mode the token is being matched in, seeded per token from the tokenize loop. Reading the command latch would answer `-1`, so the site's own syntax decides which field it means (`$mode = X` writes, anything else reads) and a site that would be both is refused. A command moves what a later read answers, as ANTLR's `mode(X)` / `pushMode(X)` do — except `popMode`, whose restore target lives on the tokenize loop's stack and not on the `Lexer`: it clears the field, and the read asserts, so that one case fails loudly instead of answering the mode just left. The read's field and seeding are emitted only for a grammar that reads `$mode`; the `$mode` write is also the only mode write that reaches the commit without passing a command method, so the commit repeats their "at most one per token" invariant only where that spelling exists.
- The lexer is a single context (mirroring the parser): it carries the input, member state, the action-effect latches, and the print sink, so actions and predicates reach everything through one handle.
- Print output belongs to the whole tokenization, not to one token: the sink accumulates across the run and `tokenize` hands it to the token stream. `parse` seeds the parser's sink from it, so lexer prints precede parser prints as they do on ANTLR4's stdout.

## Acceptance

- `SemPredEvalParser` / `SemPredEvalLexer` descriptors become the predicate suite.
- Descriptors whose output is action prints become comparable: run the generated parser, compare against the descriptor output.
- Composite descriptors' output comparison unblocks.
- Real-world grammars: driver tests with hand-written Wado SuperClass handlers (Rust `>>` splitting, TypeScript regex / division) as fixtures.
- The published jar stays the black-box oracle for order / count questions (license hygiene).

## Status

Design phases, tracked at the capability level (see `TODO.md` for the working task list):

- IR retention — done, byte-identical.
- Attribute resolution, value channel, and Wado (identity-translator) actions, including `@init` / `@after`, print-style actions, and the parser runtime-context API — largely done. `$<label>.text` reads a labeled rule call's own token range and `$<label>.ctx` renders that call's node, from anywhere in the invocation (both are value-struct fields), and a label resolves per alternative rather than rule-wide. The rule-invocation stack and the expected-token set (a generation-time constant, at a rule prequel) are in; the general `$ctx` typed-child access beyond a name the alternative already binds remains.
- Predicates in prediction (static dispatch, scan tournament, ATN exclusion; parser and single-pass lexer) — largely done, in a group's alternatives as in a rule's. The ATN-class lexer path refuses a predicate rather than dropping it.
- Group-scoped bodies — done. An action runs in the alternative it is written in, once per iteration under a repeat, and an alt-initial predicate joins that alternative's dispatch condition and the enclosing repeat's loop guard.
- java2wado for the corpus parser subset plus members translation — done, lexer bodies included: codegen stages every lexer action / predicate into Wado before emit, whatever the language, so the lexer emitters stay language-agnostic and a body they cannot emit is a diagnostic rather than a panic from inside one.
- Lexer actions (winner-replay: set-type / channel / skip / more / mode ops, single- and multi-alt), the lexer print sink, and action placement (mid-element, nested-group, and once per iteration under a repeat — restructured ones included — via a match replay that drops each action at its cursor) — done; the ATN-class lexer path remains.
- SuperClass effect interface — done for both recognizers. Lexer: predicate and action ops, the command surface (an action op also receives the `Lexer`), and the `emit` lifecycle hook; `RustLexer`, `TypeScriptLexer` and `ANTLRv4Lexer` run through faithful hand-written handlers, and ANTLRv4 grammars parse since `TOKEN_REF` / `RULE_REF` come from the hook. Parser: predicate ops over the recognizer, which is what runs `RustParser`'s `{this.NextGT()}?`. An operation taking arguments is wired from its call sites' literals on both sides, which is what runs `TypeScriptParser`'s `{this.p("of")}?` against a hand-written base.

## Open questions

- Where Wado actions live: in-grammar (`language = Wado`) and the SuperClass effect handler are primary. A sidecar id→snippet mapping is fragile — keep as an escape hatch only?
- SuperClass operations with arguments: call-site inference covers every real grammar; a pre-declared interface stays open for one that passes a non-literal.
- Whether a Kiln generator option may override the action language.
- Value semantics after recovery beyond default-init: is the default always right for user types?
- Predicate eval-count divergence policy: how much trace mismatch is acceptable before a descriptor is skipped rather than marked todo.
- IR details: which element a precedence option legally attaches to (confirm via the jar); whether upstream accepts `@init` / `@after` on lexer rules.
- `catch` / `finally` execution under Gale's resilient-parse model (no exceptions in Wado) — parked; the IR retains them.
- Effect-generic parse functions (user actions with real effects via handlers) — future extension.
- Java numeric promotion: java2wado does not model Java's implicit numeric widening, so mixing an `i32` token member with a wider value-channel field mismatches Wado's strict widths. No corpus grammar hits this, and the failure is a loud type error, not a silent miscompile; a proper fix threads Java's promotion rules through the emitter.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only.

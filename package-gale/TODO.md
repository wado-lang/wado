# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — dev-cycle essentials and the prediction failed approaches.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

This file lists what is **not yet done** at a behavioral level; find the code via search, not line pointers. Closed work belongs in commit history.

## Order of attack

1. **Soundness and compatibility divergence** — these mis-parse valid input, so they outrank every feature below.
2. **Structured diagnostic-to-rule identity** before the diagnostics that depend on it.
3. **A descriptor re-extract** whenever a JDK and the `vendor/antlr4` submodule are at hand. The skip buckets were re-triaged this way on 2026-07-30 and are now small; the standing value is that a re-extract is what proves an entry is still blocked rather than merely old.
4. **Stage C**, starting with the SuperClass action-op replay: the largest block, and the gate for drop-in ANTLR4 replacement.
5. Everything else, in whatever order a live case surfaces it.

The two LL-prediction gaps are deliberately parked, not queued — see below.

## Code-health bugs

Add a failing test before fixing.

### Soundness and compatibility divergence

The highest-risk bugs: a static-prediction edge or a parse/scan asymmetry that can mis-parse valid input. Several need their own focused PR with full-corpus validation rather than a quick patch (the prediction design notes the static path always has edges).

Entries state the symptom, how to reproduce it, and anything already measured — not a diagnosis or a proposed fix. A diagnosis written here reads as an instruction later, and two have been wrong: one would have broken compatibility if implemented as written, the other described a difference that did not exist.

- [ ] The opaque-rule expansion path drops at-end alternatives its template keeps: `try_expand_opaque` has none of the at-end handling `build_sll_node` grew, and its coverage check verifies only the opaque alts, so an at-end alt among the non-opaque configs leaves the emitted `Dispatch` with no branch of its own.
- [ ] A lexer alternation with a suffix keeps the first-match emit when the suffix is not peekable — a `RuleRef`, a `Repeat` (`('a'|'ab') 'bc'?`), or an alternation whose arms are not single elements. `lexer_suffix_is_peekable` is the window; outside it the arm is still chosen without consulting what follows. No corpus grammar is known to need one, and widening the window means teaching the peek emitters those shapes rather than loosening the gate.
- [ ] `\p{...}` covers the general categories only (generated from the UCD by `scripts/regen-unicode-tables.sh`). A script (`\p{Greek}`), block (`\p{InGreek}`) or binary property (`\p{Other_ID_Start}`, `\p{Pattern_Syntax}`) is rejected with "unsupported Unicode property", so a grammar ANTLR4 accepts is refused. No corpus grammar needs one — RustLexer's are in comments. The data for the rest is `Scripts.txt` / `PropList.txt` / `DerivedCoreProperties.txt`.
- [ ] A keyword rule longer than one character is admitted behind a carrier that can only ever match one: the keyword shortcut requires a later carrier covering the keyword's first char, but the classifier only runs on a span a carrier actually matched. `D : 'h' '2' ; C : . ;` makes `C` the carrier for `D`, and `C` never matches two chars, so `h2` lexes as two `C` tokens and `D` is unreachable. Reachability needs the carrier to be able to match the keyword's whole text, not just its first char.

Not a mis-parse — the tournament now backs every incomplete cascade — but the same under-approximation, and what makes that backstop load-bearing rather than unreachable:

- [ ] The SLL walk advances a `Repeat` config past the repeat as if it iterated exactly once, so a decision lookahead alone could settle costs a scan instead. `a : X+ Y | X Z` on `X X Y`: `gale dump` still reports `Dispatch[d=0] [TK_X] → Dispatch[d=1]` with branches `[TK_Y]→alt 0` and `[TK_Z]→alt 1` only, and the second `X` reaches the tournament backing the cascade rather than the branch it belongs in. Generating the "still in the loop" config would put `TK_X` on alt 0's branch and settle the rule on two tokens; `sll_dedup_by_alt` then has to stop keying on `alt_index` alone, the same merge `CLAUDE.md`'s first failed approach names. Pair any repair with a rejection-case fixture — `tests/grammars/ll_repeat_alt_gap.g4` is only the acceptance half.

### Pipeline and tooling correctness

- [ ] A rule-argument action whose host type contains `[]` (`r[int[] arr]`) ends early and its remainder leaks into the grammar text: the action stripper ends a `[...]` at the first unescaped `]`, which is right for the char sets the corpus does exercise. No corpus grammar hits this.

### Deleted-terminal rendering

`to_string_tree` matches ANTLR4's `Trees.toStringTree` everywhere except a deleted terminal: Gale prints `<skip z>` where ANTLR4 prints the bare token (`<missing X>` already matches). The marker is a deliberate extension — it is what makes Gale's own error-recovery fixtures able to tell recovery from a clean parse — so `ParseTrees/ExtraToken` sits in `[stage_b_skip]` rather than being forced to pass. Decide once: either the corpus gets an ANTLR-identical rendering mode, or the marker becomes opt-in and the recovery fixtures read the `Skip` rows structurally. This is a decision, not an implementation task — nothing else moves until it is made.

### Diagnostics and minor

- [ ] The error-fallback path puts internal constant names in user-facing "expected" lists while the normal expect path uses the token vocabulary — two error paths, two vocabularies.
- [ ] Error-token text is a message, so diagnostics read `unexpected token "unterminated string"`.
- [ ] A parse error's `expected` set is populated everywhere but rendered by nothing (the Display impl omits it).
- [ ] An empty lookahead signature is guarded on the scan side but not the parse side, where the lookahead condition would emit syntactically broken code; either the guard is dead or the parse side is missing it.
- [ ] A list-label leaf path double-bumps the inner name counter, and the group case lacks the collision rebind the leaf case has — both in the label-dedup bug class a fixture already exists for. The non-greedy transparent first iteration also dedups outer-scope bindings against a fresh counter table.

### Unchecked-argument quality nits (non-crash)

- [ ] Malformed lexer command _arguments_ are unchecked (the paren panics are fixed): `pushMode(42)` interns a mode literally named `42`, and `-> ;` yields the odd "unknown lexer command ;".

## Diagnostics & introspection

Grammar-authoring DX follow-ups, from the review in [#1246](https://github.com/wado-lang/wado/issues/1246) (closed — these are what it left behind). Take them in this order; the first is a prerequisite for the second.

- **Structured diagnostic-to-rule identity.** A diagnostic's owning rule is carried today as the free-form human label the warn was raised with, and tooling re-associates it by substring-matching the quoted rule name — so group-scoped warnings (no quoted rule name) never inline under their owning rule. Carry a structured owner (rule name / index), set it at the warn site, and compare by equality, keeping the label display-only. The same change lets the overlap-dispatch builder be told explicitly whether it is on the scan pass instead of recovering that from a label suffix.
- **Optional-scan-guard-fallback warning.** Lowering warns on an overlap tournament today; the obvious next warning fires when an `e?` resolves to a scan-guarded optional (live case: a rule in `example/Wado.g4`). Deferred because it needs the enclosing rule name available at the warn site; add the diagnostic kind, the warn, and a fixture together.

## Stage C — action / predicate execution

Design in [`action.md`](./action.md). The largest remaining block, and a hard prerequisite for treating Gale as a drop-in ANTLR4 replacement, for any lexer-level optimization (a fast tokenizer is meaningless if it tokenizes incorrectly), and for `superClass` / `tokenVocab`. It also unblocks composite-descriptor output comparison and parser descriptors whose output is purely action-print stdout.

Gale still silently discards action / predicate contents for the real-world grammars whose constructs the parser subset does not yet cover (`ANTLRv4Lexer`, `RustLexer`, `RustParser`, `TypeScriptLexer`, `TypeScriptParser`): they load cleanly, but the generated recognizer behaves as if every predicate were `true` and every action a no-op. That is wrong for:

- Rust's `>>` / `>>=` token splitting in generics (`{this.NextGT()}?`) and float-literal disambiguation (`{this.FloatLiteralPossible()}?`); without them Gale mis-parses nested generics. (Raw-string `#`-count matching is _not_ a Stage C case — it is a recursive fragment, an ATN-class lexer concern.)
- TypeScript's regex-vs-division disambiguation and other context-sensitive lexer and parser rules.

All of these call `this.<method>()` against a hand-written `superClass` base that lives outside the `.g4` — executing them needs the SuperClass mechanism, not just action translation. So that comes first:

- **The SuperClass effect interface** for those grammars. Landed for **predicate-only** lexer bases, including `language = Java`: RustLexer tokenizes and parses end to end through a hand-written `impl RustLexerBase`. See `action.md` ("SuperClass — an effect interface"). Remaining before TypeScript / ANTLRv4 run: action ops (`{this.m();}` — the winner-replay path), the parser side (parser-rule superClass predicates like `{this.NextGT()}?`, currently discarded), and lifecycle hooks (`nextToken` for last-token tracking). Action-op bases stay carved out (byte-identical) until the replay path lands.

Then the paths that still warn — each surfaces `UnsupportedAction`, so a grammar that needs one is never silently wrong:

- Parser actions on a non-transparent group's alternatives (the transparent path inlines its actions with its elements), an LR suffix, and a multi-alt prequel.
- Lexer actions under a `Repeat`. The action replay places each action at the cursor it was written at, covering mid-element and nested-group placement, but a `Repeat` matches an unknown number of times and the non-greedy / lookahead-aware emitters restructure the sequence around it. An alt carrying one keeps the flat emit: top-level actions run at the end of the match, anything nested inside warns.

Then the surface gaps:

- The rest of the lexer `$`-attribute surface — `$type` and member methods reading match position / text. The char-position half is covered: java2wado resolves `getCharPositionInLine()` and `_tokenStartCharPositionInLine`, but only in a Java body; the identity translator still has no `$`-form for either.
- `@lexer::members` for a `language = Java` grammar. A Java member method takes `&mut self`, but a lexer predicate runs inside `try_<rule>(lx: &Lexer, ...)` — the tournament must not mutate through a losing candidate. Java lexer bodies therefore see no members, and a reference is reported. Wiring them needs a split between members a predicate may read and members only an action may touch.
- The recognizer accessors ANTLR exposes to an action that Gale does not model: `getExpectedTokens()` and `getVocabulary()` (live case: the `ParserErrors/LL1ErrorInfo` descriptor, one of the `[stage_c_todo]` entries, prints the expected set), and `PredictionMode` / `dumpDFA`, which describe ANTLR's simulator rather than the grammar — decide whether those two are ever in scope.
- Two same-named rule labels bound to _different_ rules in different alternatives (`x=a | x=b`). Per-alternative resolution disambiguates token-vs-rule, which is all the binding records, so a `.field` read still resolves against the first-declared rule's value channel. `$<label>.text` is unaffected — it reads the call's own span.
- The ATN-class lexer path.
- java2wado numeric promotion: an `i32` token member (`$X.int` / `.type` / `.line` / `.pos` / `.index`) mixed with a wider value-channel field (`returns [long v]` / `[float]` / `[double]`) mismatches Wado's strict widths, since Wado has no implicit widening. Loud compile error, not silent; no corpus grammar hits it — lowest priority here. A proper fix threads Java's promotion rules through the translator.

And the corpus side, which is extractor work rather than codegen work (see "Descriptor corpus" below):

- The output-compare itself has landed across the parser categories (`FullContextParsing`, `LeftRecursion`, `ParseTrees`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, `Sets`), and lexer action output is compared by Stage A claim (d) instead. What is left is not more categories but the gaps above: the five `[stage_c_todo]` entries, and the descriptors that auto-skip because their action bodies hit a path that still warns.

## Descriptor corpus — coverage and re-triage

The Stage B′ JVM-oracle infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md)) is in place and its pinned trees all pass — `[stage_b_oracle_todo]` is empty, so no prediction divergence is currently pinned there. Java is needed only at extract time, not in CI; the extract also needs the `vendor/antlr4` submodule initialized.

`[stage_b_oracle_skip]` has been re-triaged (2026-07-30) and is down to the seven descriptors whose oracle output is not a valid pin at all — TestRig encodes non-ASCII as `?` while Gale renders the real code points, so pinning would strictly worsen Gale. Those are permanent unless the oracle's output encoding is fixed upstream; nothing else is parked there.

Stage B′ is the **fallback** for descriptors Stage B cannot compare, not a parallel pin: the oracle manifest is written only on the paths where the descriptor's own `[output]` is not a tree Stage B can use. So a category having no `stage_b_oracle/` directory is not by itself a gap — it can equally mean every comparable descriptor is already covered by Stage B directly. Read coverage per descriptor, not per directory.

Remaining:

- **The `[skip]` bucket is down to three, each held by a directive that changes what the parser produces**: `ParseTrees/AltNum` (`contextSuperClass` + `<TreeNodeWithAltNumField>` render alt numbers into node names), `ParserExec/ParserProperty` (`<ParserPropertyMember()>` declares the member a semantic predicate calls), `LexerExec/PositionAdjustingLexer` (`<PositionAdjustingLexer()>` overrides `nextToken()`). Expanding any of them away would leave a test that no longer tests what the descriptor is for, so each needs the host-side construct genuinely modelled — or the judgement that it is target-language-specific and stays skipped.
- **Stage B compares its expected trees through `normalize_tree`.** Stage B′ no longer does — it lost a real divergence that way (a token whose own text ends in a space). Stage B is exposed to the same class of masking; no committed Stage B expected tree currently contains whitespace inside token text, so this is latent rather than live.

### Composite (slave-grammar) descriptors

Every `CompositeLexers` / `CompositeParsers` descriptor short-circuits on the presence of imported slave grammars. Independent blockers:

- **Importer multi-input plumbing.** A grammar import (`import S;`) must resolve against the sibling slave-grammar files. Kiln already supports multi-input; lift the short-circuit once resolution lands. Actionable on its own, ahead of Stage C.
- **Host-side output (Stage C).** Every composite descriptor's expected output is a host-side artefact — action prints, token dumps, or empty — so none survive the Stage B output normalizer. Re-evaluate once Stage C lands.

## gale-highlight — theme vocabulary

`gale-highlight` provides a grammar-agnostic `Theme` (capture-class → CSS color), `stylesheet`, and `default_theme`. The _color → class_ half is covered; the _rule → class_ half is not: the set of capture classes is grammar-defined (the `.scm` highlight query), so a theme author keys colors by hand-typed class names with nothing to validate against, and a class the grammar emits but the theme omits is silently unstyled.

- **Expose a grammar's capture vocabulary from the generator.** Gale already parses the `.scm` at generation time; emit the capture names it uses as a generated artefact (e.g. a `pub const CAPTURES: List<String>`, or a class → default-color map) alongside the parser. A grammar package could then build or validate a `Theme` against the real class set, turning a mistyped or dropped class into a signal instead of silent unstyled output.

## LL prediction — parked gaps

Not queued work: both are known edges of the static path, and the complete answer is the runtime ATN simulator (`AGENTS.md` records three over-broad static repairs that each silently broke a real grammar). Revisit only when a descriptor or a real grammar surfaces a regression, and pair any repair with a rejection-case fixture.

### Iter-body K-prefix for `Repeat` inner rule references

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alternative position, but a rule reference inside a `Repeat` body still falls back to the 1-token mask path. The fixed-point "next iteration | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed.

### Multi-alt rule-reference expansion in the caller-side mask analysis

The K-prefix caller-side mask analysis halts at a multi-alternative rule reference because a per-depth union of the alternatives' prefixes would over-yield by matching cross-alternative sequences no real alternative admits. A per-alternative sequence representation could extend the walk safely — useful when a caller's continuation passes through a multi-alternative rule like `expr : literal | name`.

## Performance

Runtime performance — the benchmark state, the live profile, the directions that would move the needle, and measured dead-ends (e.g. data-driven scan) — lives in [`perf.md`](./perf.md).

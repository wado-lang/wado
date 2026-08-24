# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — dev-cycle essentials and the prediction failed approaches.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

This file lists what is **not yet done** at a behavioral level; find the code via search, not line pointers. Closed work belongs in commit history.

## Order of attack

1. **Soundness and compatibility divergence** — these mis-parse valid input, so they outrank every feature below. One entry, blocked on ICU.
2. **A descriptor re-extract** whenever a JDK and the `vendor/antlr4` submodule are at hand. The skip buckets were re-triaged this way on 2026-08-21 and are now small; the standing value is that a re-extract is what proves an entry is still blocked rather than merely old.
3. **Stage C**: the largest block, and the gate for drop-in ANTLR4 replacement. What is left is the paths that still warn and the surface gaps below.
4. Everything else, in whatever order a live case surfaces it.

The two LL-prediction gaps are deliberately parked, not queued — see below.

## Code-health bugs

Add a failing test before fixing.

### Soundness and compatibility divergence

The highest-risk bugs: a static-prediction edge or a parse/scan asymmetry that can mis-parse valid input. Several need their own focused PR with full-corpus validation rather than a quick patch (the prediction design notes the static path always has edges).

Entries state the symptom, how to reproduce it, and anything already measured — not a diagnosis or a proposed fix. A diagnosis written here reads as an instruction later, and two have been wrong: one would have broken compatibility if implemented as written, the other described a difference that did not exist.

- [ ] **Blocked on ICU.** A rule name whose first character is outside ASCII is rejected: `ÀBC : [0-9]+ ;` fails as `unexpected character "À"`, though ANTLR4's `NameStartChar` admits `\u00C0` upwards. Widening the g4 lexer's identifier predicates alone is not the fix and was measured as such: the grammar then parses, but `is_lexer_rule_name` asks `is_ascii_uppercase` where ANTLR asks `Character.isUpperCase`, so `ÀBC` silently becomes a parser rule — accepting by guessing, which the compatibility principle forbids. Both halves land together once `char::is_uppercase` exists (see Stage C below). No corpus grammar hits this.

### Pipeline and tooling correctness

Empty right now.

## Stage C — action / predicate execution

Design in [`action.md`](./action.md). The largest remaining block, and a hard prerequisite for treating Gale as a drop-in ANTLR4 replacement, for any lexer-level optimization (a fast tokenizer is meaningless if it tokenizes incorrectly), and for `superClass` / `tokenVocab`. It also unblocks composite-descriptor output comparison and parser descriptors whose output is purely action-print stdout.

The SuperClass mechanism is in place for both recognizers — `action.md` ("SuperClass — an effect interface") is its design — so `RustParser`'s `{this.NextGT()}?` runs against a hand-written base. `TypeScriptParser` does not: its predicates pass arguments (`{this.p("of")}?`), which leaves the base unwired with a diagnostic, since the rewrite drops them and the interface has no type to declare them with. Wiring one needs an explicit signature source (see `action.md`, open questions).

The paths that still warn — each surfaces `UnsupportedAction`, so a grammar that needs one is never silently wrong:

- Parser actions on a non-transparent group's alternatives (the transparent path inlines its actions with its elements), an LR suffix, and a multi-alt prequel.
- Lexer actions under a `Repeat`. The action replay places each action at the cursor it was written at, covering mid-element and nested-group placement, but a `Repeat` matches an unknown number of times and the non-greedy / lookahead-aware emitters restructure the sequence around it. An alt carrying one keeps the flat emit: top-level actions run at the end of the match, anything nested inside warns.

Then the surface gaps:

- The rest of the lexer `$`-attribute surface — `$type` and member methods reading match position / text. The char-position half is covered: java2wado resolves `getCharPositionInLine()` and `_tokenStartCharPositionInLine`, but only in a Java body; the identity translator still has no `$`-form for either.
- `@lexer::members` for a `language = Java` grammar. A Java member method takes `&mut self`, but a lexer predicate runs inside `try_<rule>(lx: &Lexer, ...)` — the tournament must not mutate through a losing candidate. Java lexer bodies therefore see no members, and a reference is reported. Wiring them needs a split between members a predicate may read and members only an action may touch.
- The recognizer accessors ANTLR exposes to an action that Gale does not model: `getExpectedTokens()` and `getVocabulary()` (live case: the `ParserErrors/LL1ErrorInfo` descriptor, one of the `[stage_c_todo]` entries, prints the expected set), and `PredictionMode` / `dumpDFA`, which describe ANTLR's simulator rather than the grammar — decide whether those two are ever in scope.
- Two same-named rule labels bound to _different_ rules in different alternatives (`x=a | x=b`). Per-alternative resolution disambiguates token-vs-rule, which is all the binding records, so a `.field` read still resolves against the first-declared rule's value channel. `$<label>.text` is unaffected — it reads the call's own span.
- `char::is_uppercase` in the Wado prelude — **blocked on ICU**. ANTLR retypes a grammar's rule name by `Character.isUpperCase`, and `NameStartChar` admits `\u00C0` upwards, so an ANTLRv4 base can only answer for ASCII names. The `Uppercase` property belongs to `core:icu` ([WEP: `core:icu`](../docs/wep-2026-08-09-core-icu.md) — the `properties` interface carries it, and character-property tries are ~44 KB of the spike's data); a UCD table generated into the prelude alongside it would be a second source of truth. Start when `wado-bundled-icu/` is wired.
- The ATN-class lexer path.
- java2wado numeric promotion: an `i32` token member (`$X.int` / `.type` / `.line` / `.pos` / `.index`) mixed with a wider value-channel field (`returns [long v]` / `[float]` / `[double]`) mismatches Wado's strict widths, since Wado has no implicit widening. Loud compile error, not silent; no corpus grammar hits it — lowest priority here. A proper fix threads Java's promotion rules through the translator.

And the corpus side, which is extractor work rather than codegen work (see "Descriptor corpus" below):

- The output-compare itself has landed across the parser categories (`FullContextParsing`, `LeftRecursion`, `ParseTrees`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, `Sets`), and lexer action output is compared by Stage A claim (d) instead. What is left is not more categories but the gaps above: the five `[stage_c_todo]` entries, and the descriptors that auto-skip because their action bodies hit a path that still warns.

## Descriptor corpus — coverage and re-triage

The Stage B′ JVM-oracle infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md)) is in place and its pinned trees all pass — `[stage_b_oracle_todo]` is empty, so no prediction divergence is currently pinned there. Java is needed only at extract time, not in CI; the extract also needs the `vendor/antlr4` submodule initialized.

`[stage_b_oracle_skip]` has been re-triaged (2026-08-21) and is down to the seven descriptors whose oracle output is not a valid pin at all — TestRig encodes non-ASCII as `?` while Gale renders the real code points, so pinning would strictly worsen Gale. Those are permanent unless the oracle's output encoding is fixed upstream; nothing else is parked there.

Stage B′ is the **fallback** for descriptors Stage B cannot compare, not a parallel pin: the oracle manifest is written only on the paths where the descriptor's own `[output]` is not a tree Stage B can use. So a category having no `stage_b_oracle/` directory is not by itself a gap — it can equally mean every comparable descriptor is already covered by Stage B directly. Read coverage per descriptor, not per directory.

Remaining:

- **Pin the `superClass` lexers as their own Stage B′ key.** `antlr4-oracle.sh --super` now answers for `RustLexer` against the same base class `driver_cst_rust_test` models, so its token stream can be oracle-pinned the way `sqlite` and `json` pin trees. `regen-oracle.sh` pins `to_string_tree()` output only, so a token-stream key is new plumbing rather than config.
- **`TypeScriptLexer` and `ANTLRv4Lexer` have no oracle at all** until each has a base class on both sides. The Wado `impl` exists for both (in their driver tests), but the `tests/grammars/java/` twin does not, and each port still has one gap a pin would fix in place: ANTLR4's retypes a rule name by `is_ascii_uppercase` where upstream asks `Character.isUpperCase` (marked `#[TODO]` in its driver test, blocked on ICU as above), and TypeScript's approximates `IsStrictMode`, which has no lexer-visible answer. `--probe-super` does not substitute; until then those grammars are pinned only by parse-success.
- **The `[skip]` bucket is down to three, each held by a directive that changes what the parser produces**: `ParseTrees/AltNum` (`contextSuperClass` + `<TreeNodeWithAltNumField>` render alt numbers into node names), `ParserExec/ParserProperty` (`<ParserPropertyMember()>` declares the member a semantic predicate calls), `LexerExec/PositionAdjustingLexer` (`<PositionAdjustingLexer()>` overrides `nextToken()`). Expanding any of them away would leave a test that no longer tests what the descriptor is for, so each needs the host-side construct genuinely modelled — or the judgement that it is target-language-specific and stays skipped.
- **Stage B compares its expected trees through `normalize_tree`.** Stage B′ no longer does — it lost a real divergence that way (a token whose own text ends in a space). Stage B is exposed to the same class of masking; no committed Stage B expected tree currently contains whitespace inside token text, so this is latent rather than live.

### Composite (slave-grammar) descriptors

Every `CompositeLexers` / `CompositeParsers` descriptor short-circuits on the presence of imported slave grammars. Independent blockers:

- **Importer multi-input plumbing.** A grammar import (`import S;`) must resolve against the sibling slave-grammar files. Kiln already supports multi-input; lift the short-circuit once resolution lands. Actionable on its own, ahead of Stage C.
- **Host-side output (Stage C).** Every composite descriptor's expected output is a host-side artefact — action prints, token dumps, or empty — so none survive the Stage B output normalizer. Re-evaluate once Stage C lands.

## LL prediction — parked gaps

Not queued work: both are known edges of the static path, and the complete answer is the runtime ATN simulator (`AGENTS.md` records three over-broad static repairs that each silently broke a real grammar). Revisit only when a descriptor or a real grammar surfaces a regression, and pair any repair with a rejection-case fixture.

### Iter-body K-prefix for `Repeat` inner rule references

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alternative position, but a rule reference inside a `Repeat` body still falls back to the 1-token mask path. The fixed-point "next iteration | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed.

### Multi-alt rule-reference expansion in the caller-side mask analysis

The K-prefix caller-side mask analysis halts at a multi-alternative rule reference because a per-depth union of the alternatives' prefixes would over-yield by matching cross-alternative sequences no real alternative admits. A per-alternative sequence representation could extend the walk safely — useful when a caller's continuation passes through a multi-alternative rule like `expr : literal | name`.

## Performance

Runtime performance — the benchmark state, the live profile, the directions that would move the needle, and measured dead-ends (e.g. data-driven scan) — lives in [`perf.md`](./perf.md).

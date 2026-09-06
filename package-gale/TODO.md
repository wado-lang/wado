# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — dev-cycle essentials and the prediction failed approaches.
- [`import.md`](./import.md) — grammar composition: how `import S;` resolves and what a delegate contributes.
- [`perf.md`](./perf.md) — runtime performance: benchmark state, live profile, what would move the needle, and measured perf dead-ends.

This file lists what is **not yet done** at a behavioral level; find the code via search, not line pointers. Closed work belongs in commit history.

## Code-health bugs

Add a failing test before fixing.

### Soundness and compatibility divergence

The highest-risk bugs: a static-prediction edge or a parse/scan asymmetry that can mis-parse valid input. Several need their own focused PR with full-corpus validation rather than a quick patch (the prediction design notes the static path always has edges).

Entries state the symptom, how to reproduce it, and anything already measured — not a diagnosis or a proposed fix. A diagnosis written here reads as an instruction later, and two have been wrong: one would have broken compatibility if implemented as written, the other described a difference that did not exist.

- [ ] **Blocked on ICU.** `ÀBC : [0-9]+ ;` is rejected as `unexpected character "À"`, though ANTLR4's `NameStartChar` admits `\u00C0` upwards. Measured: widening the g4 lexer's identifier predicates alone makes it parse, but `is_lexer_rule_name` then reads `ÀBC` as a _parser_ rule — it asks `is_ascii_uppercase` where ANTLR asks `Character.isUpperCase`. Both halves need `char::is_uppercase` (Stage C below). No corpus grammar hits this.

### Pipeline and tooling correctness

Empty right now.

## Stage C — action / predicate execution

Design in [`action.md`](./action.md). Landed: every lexer emit the match can take replays its translatable actions in place (restructured repeats included), the lexer `$`-attribute surface answers in a `language = Wado` body, `@lexer::members` works under `language = Java`, and a same-named label resolves against the rule its own alternative called. A body the translator refuses is still reported and dropped rather than replayed — `UnsupportedAction`, warn-and-emit, unchanged. What is left below is held by something other than action execution.

One narrow gap remains in the surface itself: `$line` has no `$`-form, because the inlined runtime carries no line-number helper and adding one would land in every generated parser for an attribute no grammar asks for. A reference to it is a loud error.

### Not Stage C, and blocked

- Both remaining `[stage_c_todo]` entries are held by something other than action execution, so no amount of Stage C work closes them: `FullContextParsing/AmbiguityNoLoop` is ambiguity _reporting_ (its `@init` asks for `LL_EXACT_AMBIG_DETECTION`), and `ParseTrees/ExtraTokensAndAltLabels` is the recovery divergence its own triage line describes.
- `PredictionMode` / `dumpDFA` describe ANTLR's simulator rather than the grammar, so an action printing one is out of scope for good (`action.md`, "Runtime context API").
- `char::is_uppercase` in the Wado prelude — **blocked on ICU**. ANTLR retypes a grammar's rule name by `Character.isUpperCase`, and `NameStartChar` admits `\u00C0` upwards, so an ANTLRv4 base can only answer for ASCII names. The `Uppercase` property is `core:icu`'s ([WEP: `core:icu`](../docs/wep-2026-08-09-core-icu.md), `properties` interface); a UCD table generated into the prelude beside it would be a second source of truth. Start when `wado-bundled-icu/` is wired.
- The ATN-class lexer path proper: predicates evaluated inside `latn_match` rather than refused, which needs a predicate transition in the blob and in the simulator. Do it when a grammar asks; the refusal above keeps the meantime honest.
- java2wado numeric promotion: an `i32` token member (`$X.int` / `.type` / `.line` / `.pos` / `.index`) mixed with a wider value-channel field (`returns [long v]` / `[float]` / `[double]`) mismatches Wado's strict widths, since Wado has no implicit widening. Loud compile error, not silent; no corpus grammar hits it — lowest priority here. A proper fix threads Java's promotion rules through the translator.

And the corpus side, which is extractor work rather than codegen work (see "Descriptor corpus" below):

- The output-compare itself has landed across the parser categories (`FullContextParsing`, `LeftRecursion`, `ParseTrees`, `ParserErrors`, `ParserExec`, `SemPredEvalParser`, `Sets`), and lexer action output is compared by Stage A claim (d) instead. What is left is not more categories: 103 output-compare drivers are emitted, and the two `#[TODO]` among them are the `[stage_c_todo]` entries above.

## Descriptor corpus — coverage and re-triage

The Stage B′ JVM-oracle infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md)) is in place and its pinned trees all pass — `[stage_b_oracle_todo]` is empty, so no prediction divergence is currently pinned there. Java is needed only at extract time, not in CI; the extract also needs the `vendor/antlr4` submodule initialized. Re-extract whenever both are at hand — it is what proves an entry is still blocked rather than merely old. Last full run 2026-09-01, all three phases, 98/98 oracle invocations clean; the 2026-09-05 re-extract for `import S;` ran phase 1 only and touched nothing outside the two composite categories.

`[stage_b_oracle_skip]` has been re-triaged (2026-08-24) and is down to the seven descriptors whose oracle output is not a valid pin at all — TestRig encodes non-ASCII as `?` while Gale renders the real code points, so pinning would strictly worsen Gale. Those are permanent unless the oracle's output encoding is fixed upstream; nothing else is parked there.

Stage B′ is the **fallback** for descriptors Stage B cannot compare, not a parallel pin: the oracle manifest is written only on the paths where the descriptor's own `[output]` is not a tree Stage B can use. So a category having no `stage_b_oracle/` directory is not by itself a gap — it can equally mean every comparable descriptor is already covered by Stage B directly. Read coverage per descriptor, not per directory.

### Claim strength — what the corpus does not compare

Every one of the 357 upstream descriptors is extracted and carries claim (a). What varies is how much the behavioural claim actually asserts, measured 2026-09-05:

| descriptors | strongest claim                                                    |
| ----------- | ------------------------------------------------------------------ |
| 254         | compares an output: tree, oracle tree, action print, or token dump |
| 35          | only that the parse fails                                          |
| 37          | only that the parse succeeds                                       |
| 31          | none; the `.g4` parses and nothing more                            |

The 37 are not a gap: each carries neither `[output]` nor `[errors]`, so the descriptor holds nothing further to compare. Of the 31, 10 are `<DumpDFA()>` ATN traces and 3 more are StringTemplate directives. Those are out of scope for the reasons above. What remains is a rule the extractor applies, not a per-descriptor accident:

- [ ] **An empty `[input]` is read as "nothing to feed the parser".** Ten descriptors lose their test to that one conjunct. Three are Lexer descriptors whose `[output]` is a real token dump for the empty string (`LexerExec/EOFByItself`, `EOFSuffixInFirstRule_1`, `EscapeTargetStringLiteral`), five are `ParserExec` ones with a non-empty expected print (`AStar_1`, `AorAStar_1`, `AorBStar_1`, `LL1OptionalBlock_1`, `ReferenceToATN_1`), and two are `ParserErrors` ones. The empty string is an input; the guard belongs on `[output]` alone. Two sites: the `!parsed.input.is_empty()` conjunct in each Stage A eligibility rule, and the shared `continue` gating Stage B / B′ / C.

- [ ] **An error descriptor's `[output]` is never compared.** 15 of the 37 `ParserErrors` / `LexerErrors` descriptors carry one alongside `[errors]` (`ParserErrors/ExtraneousInput`, `LexerErrors/DFAToATNThatFailsBackToDFA`, …), and claim (c) asserts only that the parse fails. That output is what error _recovery_ produced, which is where Gale and ANTLR4 are known to differ: Gale emits `TK_ERROR` tokens where ANTLR4 skips, and its trees carry the `<skip …>` markers `strip_skip_markers` already normalises for Stage B / C. **Design judgement:** decide what the comparison means before writing it. Either the recovery output is part of the compatibility contract, and these are 15 pins with some marked `[stage_a_todo]`, or it is not, and that answer belongs here. Do not pin it to whatever Gale currently prints.

- [ ] **A Lexer descriptor with both `[output]` and `[errors]` matches no claim.** `LexerExec/RecursiveLexerRuleRefWithWildcardPlus_2` and `..Star_2`, excluded by the `errors.is_empty()` conjunct in claim (d). Same judgement as above, in the lexer: their expected dumps are the token stream ANTLR4 produces _after_ recognition errors.

- [ ] **No claim compares diagnostics.** Six descriptors outside `ParserErrors` / `LexerErrors` carry `[errors]` that report ambiguity or a predicate result without failing the parse (`SemPredEvalParser/SimpleValidate`, `NoTruePredsThrowsNoViableAlt`, …). Claim (c) is gated away from them for good reason, since `assert !result.ok()` would be wrong, but that leaves the `[errors]` block unread. **Design judgement:** a diagnostics-comparing claim needs Gale's diagnostic text and positions to be part of the contract, which they are not today. `[errors]` also carries ANTLR-simulator vocabulary (`reportAttemptingFullContext d=1`) that has no Gale counterpart. Decide the portable subset before adding a claim.

Remaining:

- **Pin the `superClass` lexers as their own Stage B′ key.** `antlr4-oracle.sh --super` now answers for `RustLexer` against the same base class `driver_cst_rust_test` models, so its token stream can be oracle-pinned the way `sqlite` and `json` pin trees. `regen-oracle.sh` pins `to_string_tree()` output only, so a token-stream key is new plumbing rather than config.
- **`TypeScriptLexer` and `ANTLRv4Lexer` have no oracle at all** until each has a base class on both sides. The Wado `impl` exists for both (in their driver tests), but the `tests/grammars/java/` lexer twin does not (TypeScript's _parser_ base has one), and each port still has one gap a pin would fix in place: ANTLR4's retypes a rule name by `is_ascii_uppercase` where upstream asks `Character.isUpperCase` (marked `#[TODO]` in its driver test, blocked on ICU as above), and TypeScript's approximates `IsStrictMode`, which has no lexer-visible answer. `--probe-super` does not substitute; until then those grammars are pinned only by parse-success.
- **The `[skip]` bucket is down to three, each held by a directive that changes what the parser produces**: `ParseTrees/AltNum` (`contextSuperClass` + `<TreeNodeWithAltNumField>` render alt numbers into node names), `ParserExec/ParserProperty` (`<ParserPropertyMember()>` declares the member a semantic predicate calls), `LexerExec/PositionAdjustingLexer` (`<PositionAdjustingLexer()>` overrides `nextToken()`). Expanding any of them away would leave a test that no longer tests what the descriptor is for, so each needs the host-side construct genuinely modelled — or the judgement that it is target-language-specific and stays skipped.
- **Stage B compares its expected trees through `normalize_tree`.** Stage B′ no longer does — it lost a real divergence that way (a token whose own text ends in a space). Stage B is exposed to the same class of masking; no committed Stage B expected tree currently contains whitespace inside token text, so this is latent rather than live.

### Composite (slave-grammar) descriptors

`import S;` has landed; the contract is in [`import.md`](./import.md). All 17 `CompositeLexers` / `CompositeParsers` descriptors are under the ordinary eligibility rules: 15 claim (b), 2 claim (d) token dumps that pin the composite's token numbering, and 11 Stage C output-compares, none triaged. What is left:

- [ ] **`import Foo = Bar;` is a loud error and should not stay one.** ANTLR4 accepts the aliased form, so rejecting it breaks Stage A claim (a) for a grammar upstream compiles, which is why claim (a) now carves it out. Composition itself needs nothing new — the alias binds the same delegate `import Bar;` would. What is missing is the surface the alias exists for, ANTLR's `gFoo.rule` qualified references inside action bodies, and an oracle answer for the cases the corpus does not cover: two aliases of one grammar, and an alias shadowing another supplied grammar's declared name. Landing the binding without the qualified references would accept the form and silently drop what it means.
- [ ] **An embedded delegate cannot also be used on its own.** Nesting a language needs the delegate's lexer rules in a mode (`example/MiniCss.g4`), and which mode a rule sits in is decided by the file that declares it. So a delegate written to be embedded starts standalone in an empty `DEFAULT_MODE` and lexes nothing, and one written to stand alone gets swallowed by the host's catch-all rule when composed. Closing it means the host saying where a delegate's rules land — `import MiniCss in mode CSS;` or equivalent — after which the delegate stays `DEFAULT_MODE` and works both ways. `rebase_lexer_rules` already remaps a delegate's modes, so the composition side is an extra argument; the cost is `.g4` syntax ANTLR4 has no counterpart for, which is a compatibility decision rather than an implementation one.
- [ ] **A composite holds one action language; a delegate cannot bring its own.** `options { language = X }` is read per grammar into `Grammar.action_language`, but a composite is one `Grammar`, so the master's choice governs every body and a delegate written in another language is fed to the wrong translator. That is loud rather than silent — java2wado refuses a Wado body — but it stops a Java grammar importing a Wado one, which is the natural way to extend a vendored grammar. Closing it means the language travelling with each retained action / predicate / rule signature rather than sitting grammar-wide, and the translator being chosen per body instead of once per grammar (`action.md`, "IR retention", records the language tag per site already; what is grammar-wide is the selection).
- **Stage B′ does not oracle a composite.** `antlr4-oracle.sh` invokes the jar on one grammar file with no `-lib` slave lookup, so `stage_b_oracle_eligible` excludes them. Closing it means teaching the oracle script to stage the slaves beside the master and pass `-lib`. The extractor would then hand it composite candidates unchanged.
- **The dialect consumer still holds a copy.** Kiln `inputs` are relative paths inside the project, so a dialect in its own repository cannot name `Wado.g4` inside a dependency package ([WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md)). Resolution shrinks that from a drifting fork to a copy a checksum can hold. The rest is a Kiln gap, not a Gale one.

## LL prediction — parked gaps

Not queued work: both are known edges of the static path, and the complete answer is the runtime ATN simulator (`AGENTS.md` records three over-broad static repairs that each silently broke a real grammar). Revisit only when a descriptor or a real grammar surfaces a regression, and pair any repair with a rejection-case fixture.

### Iter-body K-prefix for `Repeat` inner rule references

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alternative position, but a rule reference inside a `Repeat` body still falls back to the 1-token mask path. The fixed-point "next iteration | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed.

### Multi-alt rule-reference expansion in the caller-side mask analysis

The K-prefix caller-side mask analysis halts at a multi-alternative rule reference because a per-depth union of the alternatives' prefixes would over-yield by matching cross-alternative sequences no real alternative admits. A per-alternative sequence representation could extend the walk safely — useful when a caller's continuation passes through a multi-alternative rule like `expr : literal | name`.

## Performance

Runtime performance — the benchmark state, the live profile, the directions that would move the needle, and measured dead-ends (e.g. data-driven scan) — lives in [`perf.md`](./perf.md).

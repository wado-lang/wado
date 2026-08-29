# Gale TODO

Open work towards full ANTLR4 compatibility and the performance budget it implies. Read this together with:

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract, prediction / codegen design, soundness invariants, descriptor pipeline, and triage workflow.
- [`AGENTS.md`](./AGENTS.md) — dev-cycle essentials and the prediction failed approaches.
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

The Stage B′ JVM-oracle infrastructure (design in [`antlr4-compatibility.md`](./antlr4-compatibility.md)) is in place and its pinned trees all pass — `[stage_b_oracle_todo]` is empty, so no prediction divergence is currently pinned there. Java is needed only at extract time, not in CI; the extract also needs the `vendor/antlr4` submodule initialized. Re-extract whenever both are at hand — it is what proves an entry is still blocked rather than merely old. Last run 2026-08-24, all three phases, 98/98 oracle invocations clean: the corpus regenerated byte-identically.

`[stage_b_oracle_skip]` has been re-triaged (2026-08-24) and is down to the seven descriptors whose oracle output is not a valid pin at all — TestRig encodes non-ASCII as `?` while Gale renders the real code points, so pinning would strictly worsen Gale. Those are permanent unless the oracle's output encoding is fixed upstream; nothing else is parked there.

Stage B′ is the **fallback** for descriptors Stage B cannot compare, not a parallel pin: the oracle manifest is written only on the paths where the descriptor's own `[output]` is not a tree Stage B can use. So a category having no `stage_b_oracle/` directory is not by itself a gap — it can equally mean every comparable descriptor is already covered by Stage B directly. Read coverage per descriptor, not per directory.

Remaining:

- **Pin the `superClass` lexers as their own Stage B′ key.** `antlr4-oracle.sh --super` now answers for `RustLexer` against the same base class `driver_cst_rust_test` models, so its token stream can be oracle-pinned the way `sqlite` and `json` pin trees. `regen-oracle.sh` pins `to_string_tree()` output only, so a token-stream key is new plumbing rather than config.
- **`TypeScriptLexer` and `ANTLRv4Lexer` have no oracle at all** until each has a base class on both sides. The Wado `impl` exists for both (in their driver tests), but the `tests/grammars/java/` lexer twin does not (TypeScript's _parser_ base has one), and each port still has one gap a pin would fix in place: ANTLR4's retypes a rule name by `is_ascii_uppercase` where upstream asks `Character.isUpperCase` (marked `#[TODO]` in its driver test, blocked on ICU as above), and TypeScript's approximates `IsStrictMode`, which has no lexer-visible answer. `--probe-super` does not substitute; until then those grammars are pinned only by parse-success.
- **The `[skip]` bucket is down to three, each held by a directive that changes what the parser produces**: `ParseTrees/AltNum` (`contextSuperClass` + `<TreeNodeWithAltNumField>` render alt numbers into node names), `ParserExec/ParserProperty` (`<ParserPropertyMember()>` declares the member a semantic predicate calls), `LexerExec/PositionAdjustingLexer` (`<PositionAdjustingLexer()>` overrides `nextToken()`). Expanding any of them away would leave a test that no longer tests what the descriptor is for, so each needs the host-side construct genuinely modelled — or the judgement that it is target-language-specific and stays skipped.
- **Stage B compares its expected trees through `normalize_tree`.** Stage B′ no longer does — it lost a real divergence that way (a token whose own text ends in a space). Stage B is exposed to the same class of masking; no committed Stage B expected tree currently contains whitespace inside token text, so this is latent rather than live.

### Composite (slave-grammar) descriptors

Every `CompositeLexers` / `CompositeParsers` descriptor short-circuits on the presence of imported slave grammars. Independent blockers:

- **Importer multi-input plumbing.** A grammar import (`import S;`) must resolve against the sibling slave-grammar files — `parse_delegate_grammars` advances past the names and records nothing today. Kiln already supports multi-input; lift the short-circuit once resolution lands. Actionable on its own — and next, because it blocks a consumer outside the corpus: a Wado dialect grammar has to vendor a copy of `Wado.g4` and drift from it until `import Wado;` resolves, where `mise run check-grammar` holds only the original to the compiler's parser ([WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md)).

  Hand-off — what the corpus already settles:

  - **Resolve by declared grammar name, not by filename.** A Kiln generator has no filesystem, so ANTLR4's `-lib` lookup has no analogue: an import binds to whichever supplied input declares that name in its header. The corpus forces the same answer — `DelegatorInvokesFirstVersionOfDelegateRule` writes `import S,T;` where the extracted files are `.slave1` (grammar T) and `.slave2` (grammar S). An import naming no supplied input is an error, not a silent omission.
  - **Two merges, not one.** `merge_grammars` concatenates unconditionally — right for split halves (`RustLexer.g4` + `RustParser.g4`), wrong for a delegate, which needs override-by-name and the master's name and kind kept. Partition in one pass: an input whose declared name appears in any input's import list is a delegate, everything else a split half. A grammar with no `import` must stay byte-identical through today's path.
  - **Master rules first, then delegates in import order.** Lexer precedence is rule order (`KeywordVSIDOrder` needs the master's `A : 'abc'` to beat the imported `ID : 'a'..'z'+`), and it keeps the composite's start rule the master's first rule.
  - **An overridden rule takes its pending refs with it.** `DelegatorRuleOverridesDelegate` drops a slave `b : B` that is the only reference to `B`; a surviving ref fails `check_references`. References resolve after composition, over the whole composite (`DelegatorRuleOverridesLookaheadInDelegate`).
  - **One token space.** A `tokens{}` entry must yield to a real rule anywhere in the composite; the dedup in `parse_grammar` is per-file, so `DelegatesSeeSameTokenType` composes three rules named `A` without it.
  - **Unpinned — ask the oracle, do not infer**: whether `options` (and `superClass`, read out of them) are imported at all (`ImportedGrammarWithEmptyOptions` pins only that an empty block survives), the delegate order beyond one level, and what `import Foo = Bar;` means.
  - **The exclusion is categorical**: three `slave_grammars.len() == 0` conjuncts in the Stage A eligibility rules plus one `continue` gating Stage B, Stage B′ and Stage C, all in `scripts/extract_antlr4_descriptors.wado`. Dropping them puts the 17 composite descriptors under the ordinary eligibility rules, with `inputs: [...]` added to the emitted `use`. The two `CompositeLexers` ones stay parse-only either way — a Lexer `[type]` with no token-dump `[output]` matches no claim.
  - **What it does not give.** An override replaces a rule — there is no "add an alternative", so a dialect restates any rule it extends. And Kiln `inputs` are project-relative paths, so a dialect in its own repository still holds a copy of `Wado.g4`; resolution shrinks that from a drifting fork to a copy a checksum can hold, and the rest is a Kiln gap.
- **Host-side output.** Every composite descriptor's expected output is a host-side artefact — action prints, token dumps, or empty — so none survive the Stage B output normalizer. Stage C has landed, so this is now the extractor work of comparing that output, not a codegen gap.

## LL prediction — parked gaps

Not queued work: both are known edges of the static path, and the complete answer is the runtime ATN simulator (`AGENTS.md` records three over-broad static repairs that each silently broke a real grammar). Revisit only when a descriptor or a real grammar surfaces a regression, and pair any repair with a rejection-case fixture.

### Iter-body K-prefix for `Repeat` inner rule references

The K-prefix follow-mask path closes the multi-token tail-greedy gap at the outer alternative position, but a rule reference inside a `Repeat` body still falls back to the 1-token mask path. The fixed-point "next iteration | exit-to-caller" computation that would let it gate is straightforward but not yet plumbed.

### Multi-alt rule-reference expansion in the caller-side mask analysis

The K-prefix caller-side mask analysis halts at a multi-alternative rule reference because a per-depth union of the alternatives' prefixes would over-yield by matching cross-alternative sequences no real alternative admits. A per-alternative sequence representation could extend the walk safely — useful when a caller's continuation passes through a multi-alternative rule like `expr : literal | name`.

## Performance

Runtime performance — the benchmark state, the live profile, the directions that would move the needle, and measured dead-ends (e.g. data-driven scan) — lives in [`perf.md`](./perf.md).

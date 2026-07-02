# Gale Stage C — Action / Predicate Execution Design

Design notes for executing `{ ... }` actions and `{ ... }?` semantic predicates in generated parsers. Companion to the Stage C sections of [`TODO.md`](./TODO.md) and [`antlr4-compatibility.md`](./antlr4-compatibility.md). Status: draft — being refined.

## Requirements

1. Existing Java action bodies run under Wado (java2wado). The descriptor corpus already carries plain Java action bodies (extract-time template expansion baked them in), so this covers the corpus and in-grammar Java of real-world grammars.
2. Actions can be written in Wado directly.

Non-goal: general Java-to-Wado translation. java2wado's scope is "the Java subset that appears in ANTLR action bodies, written against the ANTLR runtime API" — expressions, statements, local declarations, attribute references — translated onto Gale's action-context API.

## What the corpus actually needs

Survey results (2026-07):

- Descriptor corpus: small Java — prints (`System.out.println($e.v);`), member arithmetic (`this.i % 2 == 0`), lookahead tests (`_input.LA(2) != TParser.NL`), assignments to `returns` values (`$v = $a.v * $b.v;`). Attribute surface: `$ctx`, `$label.field`, `$TOKEN.text`, `$TOKEN.int`, `$text`, `$_p`, rule args / `returns` fields.
- Real-world grammars (RustLexer/Parser, TypeScriptLexer/Parser, ANTLRv4Lexer): every action is `{this.method()}` into a hand-written `superClass` base **outside the `.g4`**. Action translation alone runs none of them; the base class must exist in Wado.

## Architecture: four layers

1. IR retention. Carry action / predicate source (text + span + language tag) in the IR: `Element::Action` / `Element::Predicate`, `@init` / `@after` slots, rule args / `returns` / `locals` declarations, lexer-rule actions. Today the g4 parser discards all of these (`skip_braced_block`).
2. Attribute resolution (language-independent). `$x`, `$x.text`, `$ctx`, `$_p`, `$text` are ANTLR semantics, not host-language semantics. Gale scans the opaque body for `$`-references and resolves them itself; `$_p` maps to the existing `min_prec` threading.
3. Translator plugin. `translate(body, resolved_attrs) -> Wado snippet`. Ships two: identity (body is already Wado — requirement 2) and java2wado (requirement 1). Both target the same runtime API, so java2wado is mostly an API mapping table (`System.out.println` → `println`, `getText()` → ctx API, `_input.LT(k)` → token access), plus a small Java expression/statement parser.
4. Runtime layer. An action-context API in the generated parser (current token, matched text, input access) that all translated bodies call into. `superClass = Foo` generates a `Foo` trait; the user implements it in Wado — this is how the real-world grammars run (requirement 2 applied to requirement 1's gap).

## Semantic hard points

- Predicates participate in prediction. A false predicate must exclude its alternative in all three decision paths: static dispatch, the scan tournament, and the ATN simulator. This is the highest-risk work — it touches the scan/parse lockstep invariants (AGENTS.md). Predicates must be pure; Wado can enforce effect-freedom in the type system, a stronger guarantee than ANTLR's convention.
- Actions execute only in the chosen alternative during the actual parse — never during scan or prediction. Gale's scan is already side-effect-free, so this aligns structurally.
- Lexer timing: actions run when the rule wins the longest match; predicates evaluate mid-rule with position sensitivity (`getCharPositionInLine()`). Touches the single-pass DFA emitter and the LATN Pike VM.
- `returns` / rule args need a place to live: the CST is untyped `CstNode`, so rule values need generated `parse_*` signatures or a parallel value channel. Interaction with error-resilient parsing (what is `$v` after recovery?) needs a decision.
- Effects: actions that print thread an effect signature through every generated `parse_*`. v1 candidate: a fixed effect set; generic effect rows later if needed.

## Staging

- [ ] C1 — IR retention + attribute resolution + Wado actions (identity translator) + `@after` / print-style actions. Value channel for `returns` / args.
- [ ] C2 — predicates in prediction (`SemPredEvalParser` / `SemPredEvalLexer` descriptors are the acceptance suite).
- [ ] C3 — java2wado for the corpus subset.
- [ ] C4 — lexer actions / position-sensitive predicates + SuperClass trait (`tokenVocab` falls out).

## Open questions

- Where Wado actions live: in-grammar via `options { language=Wado }` (a), SuperClass trait impl (c) — both primary. Sidecar ID→snippet mapping (b) is fragile (positional IDs); keep as escape hatch only?
- Predicate purity: enforce effect-free via the type system (preferred) or convention?
- Effect signature of generated parsers when actions have effects.
- `$v` semantics after error recovery.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only (documented in `src/g4/action_templates.wado`).

# Gale — Action / Predicate Execution Design

Design notes for executing `{ ... }` actions and `{ ... }?` semantic predicates in generated parsers. Companion to [`TODO.md`](./TODO.md) and [`antlr4-compatibility.md`](./antlr4-compatibility.md), which track this work as the final compatibility stage. Status: draft — being refined.

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

## IR shape (Phase 1 draft)

Today the g4 parser discards every action-related construct before the IR (`skip_braced_block` / `skip_arg_action`); only `OptionValue::Action(String)` and `NamedAction { scope, name }` survive. This section defines the retention shape. Surface IR (`ir.wado`) only — GIR threading is Phase 1 follow-up.

### The common fragment type

```wado
/// A verbatim host-language fragment (action body, predicate body,
/// arg-action expression list). Text between the delimiters, delimiters
/// excluded. The language is grammar-wide (see Language below), not
/// per-fragment.
pub struct ActionSource {
    pub text: String,
    /// Source span in the .g4, for diagnostics and sidecar anchoring.
    pub span: Span,
    /// Stable id assigned by a normalize pass on the `generate()` path
    /// (same pattern as `atn_call_site`); names the emitted action fn and
    /// diagnostic anchors. -1 until assigned.
    pub action_id: i32 = -1,
}
```

Embedded directly at each site (value semantics), not interned into a grammar-level table — `merge_grammars` then needs no index remapping; the id pass provides global identity afterwards.

### Element position (parser alternatives)

```wado
pub variant Element {
    // ... existing 8 variants ...
    /// `{ ... }` element action; executes when the parse reaches this point.
    Action(ActionElement),
    /// `{ ... }?` semantic predicate; gates prediction (Phase 2).
    Predicate(PredicateElement),
}

pub struct ActionElement    { pub source: ActionSource, pub options: List<GrammarOption> }
pub struct PredicateElement { pub source: ActionSource, pub options: List<GrammarOption> }
```

Element options (`{...}?<fail='msg'>`) move onto the element instead of today's promote-to-alternative hack (`parse_alternative`); during migration the LR rewriter reads both places.

Position among elements is meaningful and preserved: an alt-initial `Predicate` is a prediction gate; a mid-alt `Action` runs after the preceding element is matched (`LexerExec/ActionPlacement` pins the lexer analog).

### Rule signature and prequels

```wado
/// One declaration in `[...]` / `returns [...]` / `locals [...]`.
/// `type_text` is opaque host text ("int", "List<String>"); splitting the
/// list is ANTLR attr-def syntax (angle-aware comma split), not host parsing.
pub struct AttrDecl { pub type_text: String, pub name: String, pub span: Span }

pub struct CatchClause { pub decl_text: String, pub body: ActionSource }

// ParserRule gains:
pub args: List<AttrDecl>,
pub returns_decls: List<AttrDecl>,
pub locals_decls: List<AttrDecl>,
pub init_action: Option<ActionSource>,    // @init
pub after_action: Option<ActionSource>,   // @after
pub throws: List<String>,
pub catches: List<CatchClause>,           // retained, execution parked
pub finally_action: Option<ActionSource>, // retained, execution parked
```

`catch` / `finally` are retained for IR completeness but not executed in any current phase — Wado has no exceptions; mapping ANTLR's recovery hooks onto Gale's resilient-parse model is a separate decision.

Call sites: `RuleRefElement` gains `pub arg_action: Option<ActionSource>` (`a[$i]`, `e[4]`).

### Lexer, named actions, options

- `LexerElement` gains the same `Action(ActionElement)` / `Predicate(PredicateElement)` variants (position-sensitive; see Semantic hard points).
- `NamedAction` gains `pub body: ActionSource` (`@members` carries member declarations the corpus needs, e.g. `int i = 0;`).
- `OptionValue::Action(String)` migrates to `OptionValue::Action(ActionSource)` for consistency.

### Language

```wado
pub variant ActionLanguage { Java, Wado, Other(String) }
// Grammar gains:
pub action_language: ActionLanguage,  // from `options { language = X }`; default Java
```

Default Java matches ANTLR (its default target) and the corpus. Gale-first grammars declare `language = Wado` in-grammar; whether a Kiln generator option may override remains open.

### Migration invariant

Phase 1a lands retention only: the g4 parser stores instead of skips, and codegen still discards — moving the "action skipped" warning from parse time to codegen time. Generated parsers stay byte-identical; the cost is mechanical (every `match` over `Element` / `LexerElement` in lower / parser_gen / prediction / dump gains arms — `Action` is ε for FIRST/nullability, `Predicate` is ε for FIRST until Phase 2). Attribute resolution then reads `ActionSource` without touching the parser again.

## Semantic hard points

- Predicates participate in prediction. A false predicate must exclude its alternative in all three decision paths: static dispatch, the scan tournament, and the ATN simulator. This is the highest-risk work — it touches the scan/parse lockstep invariants (AGENTS.md). Predicates must be pure; Wado can enforce effect-freedom in the type system, a stronger guarantee than ANTLR's convention.
- Actions execute only in the chosen alternative during the actual parse — never during scan or prediction. Gale's scan is already side-effect-free, so this aligns structurally.
- Lexer timing: actions run when the rule wins the longest match; predicates evaluate mid-rule with position sensitivity (`getCharPositionInLine()`). Touches the single-pass DFA emitter and the LATN Pike VM.
- `returns` / rule args need a place to live: the CST is untyped `CstNode`, so rule values need generated `parse_*` signatures or a parallel value channel. Interaction with error-resilient parsing (what is `$v` after recovery?) needs a decision.
- Effects: actions that print thread an effect signature through every generated `parse_*`. v1 candidate: a fixed effect set; generic effect rows later if needed.

## Staging

- [ ] Phase 1 — IR retention + attribute resolution + Wado actions (identity translator) + `@after` / print-style actions. Value channel for `returns` / args.
- [ ] Phase 2 — predicates in prediction (`SemPredEvalParser` / `SemPredEvalLexer` descriptors are the acceptance suite).
- [ ] Phase 3 — java2wado for the corpus subset.
- [ ] Phase 4 — lexer actions / position-sensitive predicates + SuperClass trait (`tokenVocab` falls out).

## Open questions

- Where Wado actions live: in-grammar via `options { language=Wado }` (a), SuperClass trait impl (c) — both primary. Sidecar ID→snippet mapping (b) is fragile (positional IDs); keep as escape hatch only?
- Predicate purity: enforce effect-free via the type system (preferred) or convention?
- Effect signature of generated parsers when actions have effects.
- `$v` semantics after error recovery.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only (documented in `src/g4/action_templates.wado`).
- IR details: which element `<p=3>` legally attaches to (confirm via jar); whether upstream accepts `@init` / `@after` on lexer rules; whether a Kiln generator option may override `action_language`.
- `catch` / `finally` execution semantics under Gale's resilient-parse model (no exceptions in Wado) — parked, IR retains them.

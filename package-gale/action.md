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
3. Translator plugin. `translate(body, resolved_attrs) -> Wado snippet`. Ships two: identity (body is already Wado — requirement 2) and java2wado (requirement 1). Both target the same runtime API, so java2wado is mostly an API mapping table plus a small Java expression/statement parser.
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

- `LexerElement` gains the same `Action(ActionElement)` / `Predicate(PredicateElement)` variants (position-sensitive; see Lexer semantics).
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

## Attribute resolution

A light scanner (same class as `action_strip` / `action_templates`: skips `'...'`, `"..."`, `//`, `/* */`) finds `$ident(.ident)?` references in an `ActionSource` and records their spans within the fragment. Resolution is per rule, against an environment built from:

- labels (`x=e`, `x+=e`) — list labels resolve to list-typed slots,
- rule args / `returns` / `locals` (`AttrDecl` names),
- unlabeled element references by name (`$ID`, `$e`; ANTLR numbers duplicates only via explicit labels, so `$e1` is just a label),
- specials: `$text`, `$ctx`, `$start`, `$stop`, `$_p` (parser), `$text`, `$type`, `$channel`, `$mode` (lexer).

Member access on a resolved token: `.text`, `.type`, `.line`, `.pos`, `.index`, `.channel`, `.int` (text parsed as integer). On a resolved rule: its declared `returns` fields plus `.text`, `.ctx`, `.start`, `.stop`.

Output is a list of `ResolvedAttr` (kind + target + replacement span). The shared substitution engine rewrites the spans to Wado expressions over the value channel / context API; the translator only translates the host code around them. An unresolvable `$ref` is a loud generation error, never a silent passthrough.

## Value channel — args / returns / locals

For each rule declaring any of the three, codegen emits a vals struct and threads it:

```wado
struct EVals { v: i64 = 0, ignored: List<String> = [] }   // returns [int v, List<String> ignored]

fn _parse_e(p: &mut Parser, min_prec: i32 = 0) -> EVals { ... }
```

- Fields are `Default`-initialized; rule args become extra `_parse_<rule>` parameters (stored into the vals struct so `$arg` and `$ret` resolve uniformly).
- The caller binds the returned vals to a local when any action references them (`let e1 = _parse_e(p);` → `$e1.v` → `e1.v`).
- Rules without declarations keep `-> ()` — actionless grammars stay byte-identical.
- Scan functions are unaffected: no values, no actions during scan.
- LR rules: the precedence-climbing loop keeps the left operand's vals in a local; `$left.v` and the corpus's ctx-cast idiom (`((BinaryContext)$ctx).e(0).v`) both map onto the lhs/rhs vals locals via a java2wado rewrite pattern. General `$ctx`-mediated child value access is out of scope (the CST stays untyped); the corpus only uses the LR-binary shape.

## Runtime context API

Actions and predicates call into a small API surface; both Wado-written and translated bodies target it. Parser side (methods on `Parser`):

| API                    | Backs                                                           |
| ---------------------- | --------------------------------------------------------------- |
| `p.la(k)` / `p.lt(k)`  | `_input.LA(k)` / `_input.LT(k)` (token kind / index)            |
| `p.token_text(i)`      | `.getText()` on a token                                         |
| `p.rule_text()`        | `$text` — input consumed by the current rule so far             |
| `p.input_text()`       | `_input.getText()` — the whole input                            |
| `p.expected_names()`   | `getExpectedTokens().toString(getVocabulary())`                 |
| `p.rule_string_tree()` | `$ctx.toStringTree(this)` — renders the node under construction |
| `p.emit(s)`            | action prints (see Effects and printing)                        |

Lexer side (`lx.text()`, `lx.column()`, `lx.token_start_column()`, `lx.set_type(TK_X)`, `lx.set_channel(n)`, `lx.push_mode(m)` / `lx.pop_mode()` / `lx.set_mode(m)`, `lx.skip()`, `lx.more()`).

Harness knobs from the corpus map to the nearest Gale equivalent or a documented no-op: `BuildParseTrees` (no-op — trees are always built), `LL_EXACT_AMBIG_DETECTION` / `DumpDFA` (no-op — diagnostics differ), `BailErrorStrategy` (`max_errors = 1`).

## Translator interface

Translation runs at generation time (inside the Kiln invocation — deterministic, pure). Sketch:

```wado
trait ActionTranslator {
    fn translate_action(&self, body: &ResolvedFragment) -> Result<String, TranslateError>;
    fn translate_predicate(&self, body: &ResolvedFragment) -> Result<String, TranslateError>;
    fn translate_member_decls(&self, body: &ResolvedFragment) -> Result<MemberDecls, TranslateError>;
    fn translate_type(&self, type_text: &String) -> Result<String, TranslateError>;
    fn translate_call_args(&self, body: &ResolvedFragment) -> Result<List<String>, TranslateError>;
}
```

`ResolvedFragment` is the `ActionSource` text plus its `ResolvedAttr` spans (already mapped to Wado expressions). A translation failure is a generation diagnostic with the fragment's span — loud, never a silent no-op. Execution is gated by a generator option (`execute_actions`, default off) until the phases complete; the default then flips, since drop-in compatibility is the end state.

- Identity translator (`language = Wado`): body and types pass through; only `$`-spans are substituted.
- java2wado (`language = Java`): a small Java parser covering — statements: expression statements, local variable declarations, assignments (incl. compound `+=`), `assert`; expressions: literals, arithmetic / comparison / logical / `%`, string concatenation, mapped method calls, `this.field`, ctx casts in the LR-binary pattern. API map: `System.out.println/print` → `p.emit`, `getText()` → context API, `_input.LA/LT`, `getCharPositionInLine()` / `_tokenStartCharPositionInLine`, lexer `setType/setChannel/skip/more/pushMode/popMode`, `toStringTree(this)` → `p.rule_string_tree()`. Type map: `int` → `i32`, `boolean` → `bool`, `String` → `String`, `List<String>` → `List<String>`, `void` → `()`. Anything outside this subset is a loud error; the subset grows on corpus demand.

Members (`@members` / `@parser::members` / `@lexer::members`): field declarations become fields on the generated `Parser` / `Lexer` struct (with translated initializers); method declarations become methods (`this.x` → `self.x`). `@header` is meaningful only under the identity translator (module-level Wado, e.g. imports); Java `@header` imports have no Wado meaning — warn and drop.

## SuperClass trait

`options { superClass = Foo }` generates `pub trait Foo` in the parser module. Method signatures are inferred from call sites: `{this.m()}?` → `fn m(&mut self, ...ctx) -> bool`, `{this.m();}` → `fn m(&mut self, ...ctx)`, where `...ctx` is a context handle exposing the runtime API above (lexer or parser view). Every corpus and real-world call is zero-argument; calls with arguments would need an explicit signature source (open — likely a sidecar or a Wado trait the user pre-declares).

The user implements the trait in Wado and passes it at the entry point: `tokenize_with<B: Foo>(input, base: &mut B)` / `parse_with<B: Foo>(...)`. Base-class state (e.g. TypeScriptLexerBase's brace/template stacks) lives in the impl (`&mut self`). A grammar with `superClass` emits only the `_with` entry points — omitting an impl is a compile error, not a silently-predicate-free parser. `tokenVocab` falls out separately: another grammar's generated `TokenKind` constants are imported by name.

## Predicates in prediction

Each predicate compiles to a standalone effect-free fn — `fn pred_<id>(p: &Parser, vals: &<Rule>Vals) -> bool` — callable from all three decision paths. Classification: context-independent (no `$arg` / `$local` / `$ret` references) vs context-dependent.

1. Static dispatch (`Direct` / `Dispatch`): an alt-initial predicate guards its branch — `if pred_<id>(...)`. Only alt-initial predicates participate in prediction (ANTLR's "visible" predicates); a mid-alt predicate evaluating false is a parse failure at that point, feeding normal recovery.
2. Scan tournament: context-independent predicates are evaluated before scanning the gated alt (a false predicate excludes it from the tournament). Context-dependent predicates are treated as true during cross-rule scans — matching ANTLR, which ignores dependent predicates evaluated outside their owning context — and evaluated for real in the owning rule's own dispatch.
3. ATN simulator: predicate-gated alternatives carry `action_id`s in the ATN blob; the simulator takes a predicate-evaluation callback (closing over `p`). Evaluation happens only when the simulated stack makes the context valid; otherwise assume true. `SemPredEvalParser` / `SemPredEvalLexer` descriptors pin the semantics.

All predicates false at a decision → "no viable alternative" diagnostic. Purity: predicate fns are declared effect-free, which the Wado type system enforces — stronger than ANTLR's convention. Ambient logging (`log_stdout`-class, no effect required) remains possible, which is exactly what the corpus's printing predicates need. Predicate evaluation counts and ordering will not match ANTLR exactly (different algorithms); descriptors that pin eval traces get `[todo]` triage with that reason — the chosen parse must match, the trace need not.

## Action execution timing

- GIR gains `ActionOp { action_id }`, emitted on the parse side only; the scan side drops it (scans stay side-effect-free).
- Speculation: Gale's hybrid dispatch has save-and-rewind paths (`try alt`). Actions must not observably execute inside an attempt that rewinds — `ActionOp` is suppressed while `p.speculating`. A mutation-dependent predicate could in principle diverge from ANTLR here (ANTLR never executes actions during prediction either, but the decision points differ); pin with descriptors if one surfaces.
- `@init` runs at rule entry before the first decision (the corpus uses it for `expected_names` and bail); `@after` runs after the body, before the node is finished (`rule_string_tree` sees the complete children).
- LR rewrite: an alt's actions move with the alt into the precedence-climbing loop; `$_p` resolves to `min_prec`.

## Effects and printing

Actions print through `p.emit(s)`: appended to a buffer on `ParseResult` (`result.output`) and mirrored to stdout when the `echo_actions` generator option is on. This keeps generated `parse_*` signatures effect-free (no effect plumbing through the recursive descent), makes descriptor `[output]` comparison a plain string equality on `result.output`, and still gives CLI users real stdout. Wado-written actions may also use ambient logging directly. Effect-generic parse functions (user actions with arbitrary effects, handled via effect handlers) are a future extension — nothing in this design blocks them.

## Error recovery and values

Recovery invariants with actions present:

- An `ActionOp` executes iff the op path reaches it normally; ops skipped by recovery skip their actions.
- Vals structs are `Default`-initialized, so `$x.v` of a missing / errored sub-rule reads the default, never traps.
- Predicates still evaluate during recovery dispatch (they gate which alt recovery resumes into).
- Diagnostics and the tree shape are unchanged from today's resilient behavior.

## Lexer semantics

- Element actions run when the rule wins the longest match, in element order, with the cursor state they were passed (`ActionPlacement` pins this); they never run for losing candidates.
- Predicates evaluate mid-match, position-sensitively (`Column() < 2`), in both the single-pass DFA emitter and the LATN Pike VM (thread-local evaluation; a false predicate kills the thread).
- Lexer commands stay typed IR (`-> skip` etc.); `lx.set_type` / `set_channel` from actions compose with them, action last-wins.

## Staging

- [ ] Phase 1 — IR retention (1a, byte-identical) + attribute resolution + value channel + Wado actions (identity translator) + `@after` / print-style actions via `p.emit`.
- [ ] Phase 2 — predicates in prediction (`SemPredEvalParser` / `SemPredEvalLexer` descriptors are the acceptance suite).
- [ ] Phase 3 — java2wado for the corpus subset + members translation.
- [ ] Phase 4 — lexer actions / position-sensitive predicates + SuperClass trait (`tokenVocab` falls out).

## Acceptance

- `SemPredEvalParser` / `SemPredEvalLexer` descriptors (today pinned `[stage_a_todo]` / oracle-skipped) become the predicate suite.
- Descriptors whose `[output]` is action prints become comparable: run the generated parser, compare `result.output` against the descriptor `[output]`. Stage A tokens tests with action-print prefixes lose their auto-`#[TODO]`.
- Composite descriptors' `[output]` comparison unblocks (their outputs are all action prints).
- Real-world grammars: driver tests with hand-written Wado SuperClass impls (TypeScript regex/division, Rust `>>` splitting) as fixtures.
- The published jar stays the black-box oracle for order/count questions (License hygiene).

## Open questions

- Where Wado actions live: in-grammar via `options { language=Wado }` and SuperClass trait impl are primary. Sidecar ID→snippet mapping is fragile (positional IDs); keep as escape hatch only?
- SuperClass methods with arguments: signature source (sidecar vs user-pre-declared trait).
- Whether a Kiln generator option may override `action_language`; interaction with `execute_actions` default flip.
- `$v` semantics after recovery beyond Default-init (is Default always right for user types?).
- Predicate eval-count divergence policy: how much trace mismatch is acceptable before a descriptor is `[skip]` instead of `[todo]`.
- IR details: which element `<p=3>` legally attaches to (confirm via jar); whether upstream accepts `@init` / `@after` on lexer rules.
- `catch` / `finally` execution semantics under Gale's resilient-parse model (no exceptions in Wado) — parked, IR retains them.
- Effect-generic parse functions (user actions with real effects via handlers) — future extension.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only (documented in `src/g4/action_templates.wado`).

# Gale — Action / Predicate Execution Design

Design notes for executing `{ ... }` actions and `{ ... }?` semantic predicates in generated parsers. Companion to [`TODO.md`](./TODO.md) and [`antlr4-compatibility.md`](./antlr4-compatibility.md), which track this work as the final compatibility stage. Status: draft — being refined.

## Requirements

1. Existing Java action bodies run under Wado (java2wado). The descriptor corpus already carries plain Java action bodies (extract-time template expansion baked them in), so this covers the corpus and in-grammar Java of real-world grammars.
2. Actions can be written in Wado directly.

java2wado's initial scope is "the Java subset that appears in ANTLR action bodies, written against the ANTLR runtime API" — expressions, statements, local declarations, attribute references — translated onto Gale's action-context API. The subset grows on corpus demand toward general Java-to-Wado translation.

## What the corpus actually needs

Survey results (2026-07):

- Descriptor corpus: small Java — prints (`System.out.println($e.v);`), member arithmetic (`this.i % 2 == 0`), lookahead tests (`_input.LA(2) != TParser.NL`), assignments to `returns` values (`$v = $a.v * $b.v;`). Attribute surface: `$ctx`, `$label.field`, `$TOKEN.text`, `$TOKEN.int`, `$text`, `$_p`, rule args / `returns` fields.
- Real-world grammars (RustLexer/Parser, TypeScriptLexer/Parser, ANTLRv4Lexer): every action is `{this.method()}` into a hand-written `superClass` base **outside the `.g4`**. Action translation alone runs none of them; the base class must exist in Wado.

## Architecture: four layers

1. IR retention. Carry action / predicate source (text + span + language tag) in the IR: a per-alternative `actions` sidecar, `@init` / `@after` slots, rule args / `returns` / `locals` declarations, lexer-rule actions. Today the g4 parser discards all of these (`skip_braced_block`).
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

### Element position — a parallel per-alternative list

Actions and predicates are **not** added as `Element` variants. Adding a variant to `Element` / `LexerElement` breaks every exhaustive match over them (~16 sites) and, worse, silently shifts the index-based LR / prediction / scan logic that reads `alt.elements[0]` (LR self-ref), `alt.elements[k+1]` (suffix walk), etc. — making byte-identical output depend on correctly skipping an invisible ε element at every one of those sites.

Instead each alternative carries a sidecar list, so `elements` stays exactly the sequence of things that match (byte-identical, no match-site touched) and position is recorded explicitly:

```wado
pub enum ActionKind { Action, Predicate }   // `{ ... }` vs `{ ... }?`

pub struct AltAction {
    pub kind: ActionKind,
    pub source: ActionSource,
    /// Number of significant elements before this action, i.e. it runs
    /// after `elements[before_index - 1]` and before `elements[before_index]`.
    /// `0` = alt-initial (a `Predicate` here is a prediction gate).
    pub before_index: i32,
}

// Alternative / LexerAlternative gain:
pub actions: List<AltAction> = [],
```

Position is fully preserved: an alt-initial predicate is `before_index == 0`; a mid-alt action after the k-th element is `before_index == k` (`LexerExec/ActionPlacement` pins the lexer analog). Phase 2 reads `actions` to place predicates in prediction and actions in the parse.

Element options (`{...}?<fail='msg'>` / `{...}<p=3>`) keep their current promote-to-alternative handling in `parse_alternative` (the LR rewriter already reads `<p=N>` / `<assoc>` from `alt.options`), so that path stays byte-identical; the action body is recorded alongside.

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

- `LexerAlternative` gets the same `actions: List<AltAction>` sidecar (position-sensitive; see Lexer semantics).
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

Phase 1a lands retention only: the g4 parser stores actions instead of skipping them, and codegen still discards. Because actions live in the `actions` sidecar rather than in `elements`, generated parsers stay byte-identical by construction — no `Element` / `LexerElement` variant is added, so no `match` in lower / parser_gen / prediction / dump changes, and the index-based LR / prediction / scan logic never sees an action. Phase 1a-i landed the out-of-element retention (rule signatures, named-action / option bodies); Phase 1a-ii adds the `actions` sidecar. Attribute resolution then reads `ActionSource` without touching the parser again.

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
- LR rules: the precedence-climbing loop keeps the left operand's vals in a local; `$left.v` and the corpus's ctx-cast idiom (`((BinaryContext)$ctx).e(0).v`) both map onto the lhs/rhs vals locals via a java2wado rewrite pattern. General `$ctx`-mediated child value access (`$ctx.childRule()`) needs typed child access over the CST — a later item (see the `$ctx` design); the corpus's LR uses only the LR-binary shape, handled here.

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

`ResolvedFragment` is the `ActionSource` text plus its `ResolvedAttr` spans (already mapped to Wado expressions). A translation failure is a generation diagnostic with the fragment's span — loud, never a silent no-op. Actions execute for grammars whose action language Gale can emit: `language = Wado` today, `language = Java` once java2wado lands. That later change makes existing Java grammars execute their actions — an intentional, correct step toward drop-in compatibility, not something to gate against.

- Identity translator (`language = Wado`): body and types pass through; only `$`-spans are substituted.
- java2wado (`language = Java`): a small Java parser covering — statements: expression statements, local variable declarations, assignments (incl. compound `+=`), `assert`; expressions: literals, arithmetic / comparison / logical / `%`, string concatenation, mapped method calls, `this.field`, ctx casts in the LR-binary pattern. API map: `System.out.println/print` → `p.emit`, `getText()` → context API, `_input.LA/LT`, `getCharPositionInLine()` / `_tokenStartCharPositionInLine`, lexer `setType/setChannel/skip/more/pushMode/popMode`, `toStringTree(this)` → `p.rule_string_tree()`. Type map: `int` → `i32`, `boolean` → `bool`, `String` → `String`, `List<String>` → `List<String>`, `void` → `()`. Anything outside this subset is a loud error; the subset grows on corpus demand.

Members (`@members` / `@parser::members` / `@lexer::members`): field declarations become fields on the generated `Parser` / `Lexer` struct (with translated initializers); method declarations become methods (`this.x` → `self.x`). `@header` is meaningful only under the identity translator (module-level Wado, e.g. imports); Java `@header` imports have no Wado meaning — warn and drop.

## java2wado — Java action translation

java2wado translates the Java subset in ANTLR action bodies to Wado, so `language = Java` grammars execute their actions. It is the identity translator's job plus a translation of the surrounding Java host code: the `$`-spans resolve to the _same_ Wado expressions over the _same_ value channel / runtime API, so nothing in the attribute layer changes. java2wado is a new front end that parses Java and re-emits Wado, filling the `$`-leaves through the shared `resolve_attr_ref`.

Design: a real (small) Java parser, not regex rewriting. String concatenation (`"a" + $x`), operator precedence, and `this.`-rewriting cannot be done soundly by span substitution. Module layout mirrors `src/g4/` (lexer / parser split): `src/java2wado.wado` (facade + the `ActionLanguage` dispatcher), then `src/java2wado/{jlexer,jparser,jemit,jmembers}.wado`. `$`-refs lex as an opaque primary token.

API / type mapping is snake_case by default — an arbitrary `@members` / superClass method can only be case-converted — plus a small table of semantic redirects and fixed recognizer methods:

| Java                                              | Wado                                     | kind              |
| ------------------------------------------------- | ---------------------------------------- | ----------------- |
| `this.foo(args)` / `foo()`                        | `this.foo(args)` (snake_case)            | default           |
| `System.out.println/print(x)`                     | `p.emit(...)` (`println` appends `\n`)   | semantic redirect |
| `x.equals(y)`                                     | `x == y`                                 | semantic redirect |
| `TParser.<TOKEN>`                                 | `TK_<TOKEN>`                             | semantic redirect |
| `getText()` / `_input.LA(k)` / `_input.getText()` | `rule_text()` / `la(k)` / `input_text()` | fixed rename      |
| `$ctx.toStringTree(this)`                         | `rule_string_tree()` (via attr engine)   | fixed rename      |
| `int` / `boolean` / `String` / `void`             | `i32` / `bool` / `String` / `()`         | type map          |

Anything outside the subset is a loud generation diagnostic carrying the fragment span — never a silent no-op; the subset grows on corpus demand. `@members` field declarations become fields on the generated `Parser` / `Lexer`, method declarations become methods (`this.` → `self.`).

The rollout is cover-then-flip with one carve-out. A Java grammar's actions were discarded (byte-identical to actionless); the plan covers the corpus Java subset, then flips Java emittable in one step (`action_language_is_emittable`). The single principled carve-out is `superClass` grammars: their actions call a hand-written base class outside the `.g4` that Gale cannot see, so translating it is the SuperClass-trait job (Phase 4), not a java2wado coverage gap. Non-superClass Java grammars — the whole descriptor corpus — flip unconditionally; any untranslatable fragment there is a loud error that forces coverage rather than hiding it.

## SuperClass trait

`options { superClass = Foo }` generates `pub trait Foo` in the parser module. Method signatures are inferred from call sites: `{this.m()}?` → `fn m(&mut self, ...ctx) -> bool`, `{this.m();}` → `fn m(&mut self, ...ctx)`, where `...ctx` is a context handle exposing the runtime API above (lexer or parser view). Every corpus and real-world call is zero-argument; calls with arguments would need an explicit signature source (open — likely a sidecar or a Wado trait the user pre-declares).

The user implements the trait in Wado and passes it at the entry point: `tokenize_with<B: Foo>(input, base: &mut B)` / `parse_with<B: Foo>(...)`. Base-class state (e.g. TypeScriptLexerBase's brace/template stacks) lives in the impl (`&mut self`). A grammar with `superClass` emits only the `_with` entry points — omitting an impl is a compile error, not a silently-predicate-free parser. `tokenVocab` falls out separately: another grammar's generated `TokenKind` constants are imported by name.

## Predicates in prediction

Each predicate compiles to a standalone effect-free fn — `fn pred_<id>(p: &Parser, vals: &<Rule>Vals) -> bool` — callable from all three decision paths. Classification: context-independent (no `$arg` / `$local` / `$ret` references) vs context-dependent.

1. Static dispatch (`Direct` / `Dispatch`): an alt-initial predicate guards its branch — `if pred_<id>(...)`. Only alt-initial predicates participate in prediction (ANTLR's "visible" predicates); a mid-alt predicate evaluating false is a parse failure at that point, feeding normal recovery.
2. Scan tournament: context-independent predicates are evaluated before scanning the gated alt (a false predicate excludes it from the tournament). Context-dependent predicates are treated as true during cross-rule scans — matching ANTLR, which ignores dependent predicates evaluated outside their owning context — and evaluated for real in the owning rule's own dispatch.
3. ATN simulator: predicate-gated alternatives carry `action_id`s in the ATN blob; because prediction predicates are alt-initial, the caller pre-evaluates each gated alt at the decision and excludes the false ones from the simulator's seed (they can never win), so the simulator itself stays a pure function of the token stream. `SemPredEvalParser` / `SemPredEvalLexer` descriptors pin the semantics.

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

- [x] Phase 1a — IR retention, byte-identical (1a-i: rule signatures, named-action / option bodies; 1a-ii: per-alternative `actions` sidecar).
- [~] Phase 1b — attribute resolution + value channel + Wado actions (identity translator) + `@after` / print-style actions via `p.emit`.
  - [x] Phase 0 — `assign_action_ids` normalize pass (stable ids, byte-identical).
  - [x] LR-alt actions: `emit_lr_branch_body` interleaves the LR alt's actions with the suffix ops (surface `before_index b` → before suffix op `b - 1`), so print / side-effect actions on `e '+' e`-style continuations run in left-associative parse order. Fixture `wado_lr_action.g4`.
  - [x] LR value channel (the corpus LR-binary shape): `rule_returns_vals` now admits LR rules. Unified model — the atom is the primary alt body, so it _receives_ the invocation's seeded `vals` (like every `_alt_<n>` helper) rather than minting its own. `gen_lr_fn` seeds `vals` at entry, threads it into the atom into `_lr_acc`, and each continuation binds the accumulated left operand under the stripped leading self-ref's name (`prefix_self_ref.name`, so `$l.v` reads it), opens a new context with a fresh `vals` the suffix action writes (`$v`), and folds it back — the trailing self-ref's own `vals` (`$r.v`) comes from its recursive call. Both static and ATN LR loops. Fixture `wado_lr_vals.g4` (`l=e op r=e {$v = $l.v op $r.v}`, precedence-correct).
  - [x] `@init` / `@after` for LR rules (mirrors ANTLR's `enterRecursionRule` / `exitRule`): `@init` seeds the invocation's `vals` at entry — the atom receives it so the primary alt's action sees the seed (`$v == <seed>`); each recursive operand is an independent invocation with its own `@init`/`@after`. `@after` (`emit_lr_after_return`) runs once at exit, binding the final `_lr_acc` as `vals` so `$v` reads (and may write) the whole expression's value. Fixture `wado_lr_prequel.g4`.
  - [x] 1b-1 — attribute scanner (`attr_scan::find_attr_refs`).
  - [x] 1b-1b — attribute resolution (`attr_resolve::resolve_attrs`, target classification + member validation, loud errors). Not yet wired into emit.
  - [x] 1b-2 — print-style Wado actions execute via `p.emit`, surfaced on `ParseResult.output` (`language = Wado`). Interleaved by `before_index` in `gen_alt_elements`, guarded on `!p.speculating`. Attribute-referencing bodies raise a loud `UnsupportedAction` diagnostic pending substitution.
  - [~] 1b-3 — value channel + attribute-substitution engine.
    - [x] Span-splicing engine (`attr_substitute::splice_attrs`).
    - [x] Identity translator (`action_translate`): resolves refs and maps the supported subset to Wado exprs; loud error otherwise.
    - [x] Own-rule value channel: a single-alt rule declaring `returns` / `locals` (defaultable types) gets a `<Rule>Vals` struct + `vals` local; `$v` / `$local` in its actions substitute to `vals.<name>`. Write-then-read across a rule's actions works end to end.
    - [x] Cross-rule `$a.v`: a value-channel rule's `_parse_<rule>` (inner + wrapper) returns `<Rule>Vals`; call sites already bind `let <name> = _parse_<rule>(p)`, so `$a.v` → `a.v`. Entry closures discard the vals (`|p| { let _ = _parse_<rule>(p); }`). The corpus LR-binary `$a.v + $b.v` shape works (non-LR).
    - [x] Multi-alt (non-LR) value channel: `rule_returns_vals` now admits any non-left-recursive rule with a defaultable `returns`/`locals` (LR excluded via `rule_is_left_recursive`). Each per-alt `_parse_<rule>_alt_N` helper and the inlined dispatch body declares its own `<Rule>Vals` local and returns it; `gen_error_fallback` / predicate-gated / speculative-commit paths all return `vals`. Channel rules are routed off the single-token compact path. Fixture `wado_multi_vals.g4`. Known gap: an action in a _non-last, partial/no-scan speculative-tournament_ alt is suppressed under speculation (pre-existing action semantics), so its `$v` writes are lost — token-led / full-scan / predicate-gated dispatch are unaffected.
    - [x] Token label member `$x.text` → `p.token_text(<index>)` (the `let x = p.expect(...)` binding).
    - [x] Rule arguments as `_parse_<rule>` params (single- **and** multi-alt, non-LR). `rule_threads_args` (defaultable arg types, non-LR) gets one defaulted parameter per arg (appended last, so the entry and plain `a` callers omit them), stored into `<Rule>Vals` at inner-fn entry so `$arg` → `vals.<name>`. The seeded `vals` (args + `@init`) threads into the multi-alt `_alt_<n>` helpers via `alt_vals_arg` (each helper takes a `vals` param and mutates its own copy), so overlap-group dispatch keeps value continuity. Call sites split `a[$i, $j]` on top-level commas and translate each against the caller's channel. Fixtures `wado_rule_arg.g4`, `wado_multi_arg.g4`. LR rule args stay a loud error.
    - [x] Other token members: `$x.int` → `p.token_int`, `.type` → `p.token_kind`, `.line` → `p.token_line`, `.pos` → `p.token_col`, `.index` → the bound index, `.channel` → 0. Unlabeled `$ID`/`$ID.member` route through the same path (the first-occurrence binding; deduped bindings below). Fixture `wado_tok_members.g4`.
    - [x] Deduped `$e.v` when a token / rule field-name collision renames the binding (`r : E e` → the rule call is `e_2`, not `e`). Codegen records the alt's actual post-dedup bindings (`collect_alt_bindings`, `GenContext.alt_bindings`, keyed by token-vs-rule kind + pre-dedup base) and the translator (`binding_for`) reads them instead of re-deriving `binding_name` — one source of truth, so `$e.v` resolves to the real local. Fixture `wado_dedup_ref.g4`.
  - [~] 1b-4 — runtime context API + prequel timing.
    - [x] `@init` (rule entry) / `@after` (after body) for single-alt rules, sharing the rule's `vals` local; execution order pinned by a driver test.
    - [x] `@init` / `@after` for multi-alt (non-LR) rules. `@after` runs at the end of each alt body (once, for the chosen alt) via `rule_after_for`, reading the alt's own `vals` — correct on inlined, singleton, and `_alt_<n>`-helper dispatch. `@init` runs at the multi-alt inner-fn entry and its `vals` seed threads into the `_alt_<n>` helpers (`alt_vals_arg`), so overlap-group dispatch preserves it. LR rules stay deferred. Fixtures `wado_multi_prequel.g4`, `wado_multi_arg.g4`.
    - [~] Runtime context API surface. `p.la(k)` / `p.lt(k)` / `p.text_span(start, end)` / `p.token_start(i)` / `p.input_text()` emitted on the generated `Parser` (under `emit_actions`), so a Wado action / predicate body calls them verbatim (`{p.la(2) != TK_NL}?`). Rule-span specials — `$text` (consumed input), `$start` / `$stop` (first / last token, member-addressable like any token) — all read one capture, `_rule_start_tok = p.pos` (`emit_span_capture`), emitted at **every** rule-body entry (single-alt, multi-alt inner, `_alt_<n>` helpers, LR inner/atom) so they resolve for every dispatch shape and for LR continuations. Fixtures `wado_rule_text.g4` (single-alt `$text`), `wado_start_stop.g4` (multi-alt `$start`/`$stop`/`$text`). `$ctx.toStringTree()` (the corpus's only `$ctx` use — 82 grammars) renders the rule node under construction, against `RULE_NAMES` + a token-name table built once into the `CTX_TOKEN_NAMES` global (emitted under `emit_actions`). A non-LR rule's node is still open in `@after` (after the body, before `finish_node`), so `p.rule_string_tree()` → `TreeBuilder::render_open`. A left-recursive rule's level is already folded shut by `@after` (precedence climbing), so `p.rule_string_tree_final()` → `render_last_finished` reads the just-closed node instead; the translator picks the method from `rule_is_left_recursive`. The call absorbs its own source syntax (`()`, `(this)`) so the ANTLR/java2wado `toStringTree(this)` idiom reduces to a bare call. Fixtures `wado_ctx_tree.g4` (non-LR, plus a recovery tree) and `wado_lr_ctx_tree.g4` (LR). General `$ctx`-mediated typed child access (`$ctx.childRule()`) is a later item (the CST is untyped).
    - [x] `$_p` → the `min_prec` parameter (`parser_special_expr`). Every generated rule fn carries `min_prec` (0 outside a precedence context), so `$_p` resolves in any rule body — the LR precedence threshold when climbing, `0` at entry. No member is allowed (`$_p.x` is a resolve error). Fixture `wado_lr_p.g4` (`_p=0` at entry, `_p=2` for the right operand).
    - [x] `@init` / `@after` for LR rules — see the LR value-channel entry above (`wado_lr_prequel.g4`).
- [~] Phase 2 — predicates in prediction (`SemPredEvalParser` / `SemPredEvalLexer` descriptors are the acceptance suite).
  - [x] Inline runtime guard for mid-alt / single-alt-rule predicates: a `{cond}?` compiles to `if !p.speculating { if !(<cond>) { p.no_viable(...); return; } }`, so a false predicate fails the parse into normal recovery. Substitution applies to the condition (identity translator).
  - [~] Prediction-time gating: alt-initial predicates choosing which alt to take (static dispatch / scan tournament / ATN), the `SemPredEvalParser` acceptance suite.
    - [x] Static dispatch, all three selection sites (`mark_gated_predicates` marks the gated ids so the in-body copy is suppressed; context-independent `$`-free predicates only):
      - `Ambiguous` overlap group whose alts are **interchangeable** (`group_alts_interchangeable` — same terminal sequence, so they genuinely tie) — the length tournament is bypassed for a grammar-order predicate chain (`if pred0 … else if pred1 … else no_viable`); an unpredicated alt is the always-viable fallback, and the now-dead per-alt scan helpers are not emitted. A group whose alts differ in length/token keeps the tournament (the chain would misroute a longer/distinct alt); its predicates stay unsupported. Fixtures `wado_pred_select.g4`, `wado_pred_mixed.g4`.
      - Token-led `Direct` branch and singleton overlap group — the alt-initial predicate folds into the branch condition (`if kind == TK_X && (pred)`), so a false predicate on a uniquely-selected alt fails into no-viable (ANTLR's "predicate tested even when unambiguous"). Fixture `wado_pred_direct.g4`.
    - [x] Single-token multi-alt rules (the `SemPredEvalParser/Simple` shape, `a : {p}? A | {q}? A | B`): a rule carrying emittable actions/predicates no longer takes the compact `all_alts_are_single_token` fast path (`rule_has_emittable_actions`), so it reaches the general path where the gating above applies. Fixture `wado_pred_single.g4`.
    - [x] Context-dependent predicates (`$local` / `$ret` / `$arg`) — an alt-initial predicate referencing the value channel is gateable: `gate_condition_for` translates its body (`$n` → `vals.n`, including threaded rule args) and folds the condition into the parse-side dispatch (token-led / singleton / interchangeable-`Ambiguous`), where the seeded `vals` is in scope. Marking and emission share `gate_condition_for`, so a body the translator cannot emit is simply not gated (it falls to the inline guard + warning). Fixtures `wado_ctx_pred.g4` (`@init`-seeded `$n`, token-led) and `wado_arg_pred.g4` (`{$mode == 1}?` on a threaded rule arg). By design a context-dependent predicate cannot gate a _scan_ (the scan side has no `vals`), so a non-interchangeable overlap group with a dependent predicate keeps the length tournament; a truly ATN-class predicate-gated decision is handled by the ATN exclusion path (below).
  - [~] Lexer semantic predicates (`SemPredEvalLexer`): a single-alt lexer rule's alt-initial (`{cond}? ...`) and trailing (`... {cond}?`) predicates emit as `if !(cond) { return -1; }` guards in the rule's match fn (`gen_lexer_rule_predicates`, gated on `emit_actions`), so a false predicate rejects the rule and the tokenizer tries the next — deciding a longest-match tie (`KW` vs `ID`) or disabling a rule (`{false}?`). The condition runs through the identity translator (`translate_lexer_predicate`) with the match fn's `chars` / `start` / `pos` in scope; a `$`-free body passes through verbatim and an unresolvable `$`-ref is a loud codegen error. Lexer `$text` resolves to the matched slice (`LexerSlice::new(chars, start, pos).to_string()`), so the `SemPredEvalLexer/EnumNotID` shape (`getText().equals("enum")` → `{$text == "enum"}?`) works in `language = Wado`. Fixtures `wado_lex_pred.g4` (`pos - start == 4`, `{false}?`), `wado_lex_text_pred.g4` (`$text`). Multi-alt lexer rules place per-alt predicates too (`gen_lexer_alts` threads `gen_lexer_rule_predicates` with a `break <label>` fail-action, so a false predicate falls through to the next alt); fixture `wado_lex_multi_pred.g4` (`KW : [a-z]+ {$text == "cat"}? | 'dog'`). Group- and fragment-inlined predicates work too: `emit_actions` (`ea`) threads through the whole lexer emit chain (`gen_lexer_alt_seq` / `gen_lexer_elem` / `gen_lexer_repeat` / `gen_lexer_non_greedy_repeat` / `gen_lexer_not` / the lookahead-aware emitter), so a predicate inside a `( … )` group or an inlined non-recursive fragment gates its branch (`break <group/alt label>`). Fixtures `wado_lex_group_pred.g4` (`KW : ([a-z]+ {$text == "cat"}? | 'dog')`), `wado_lex_frag_pred.g4` (predicate inside `fragment LETTERS`). `gen_lexer_alt_seq` now drives predicate emission over **every** `before_index` of its alt — alt-initial (`0`), mid-alt (a predicate _between_ elements), and trailing (`len`) — each with the alt's own fail-action (`return -1` single-alt rule / `break <label>` multi-alt / group / fragment). Because emission lives in `gen_lexer_alt_seq` rather than the caller, a single-alt inlined group / fragment (reached straight through it) gates on its boundary predicates too. Fixtures `wado_lex_mid_pred.g4` (`KW : [a-z] {$text == "c"}? [a-z] [a-z]`) and `wado_lex_frag_boundary_pred.g4` (trailing predicate on a single-alt `fragment`). Remaining: the LATN (ATN-class) lexer path and the rest of the lexer `$`-attribute surface (`$type` / `getCharPositionInLine`).
  - [x] `pred_<id>` effect-free standalone fns + the ATN parser predicate path (context-free and context-dependent predicates).
    - [x] Context-free gated predicates (alt-initial, `language = Wado`, `$`-free) extract to a standalone `fn pred_<id>(p: &Parser) -> bool { return <body>; }` (`gen_predicate_fns`, registered in `mark_alt_gate_predicates`, keyed by `action_id` on `GenContext.predicate_fns`). The dispatch calls `pred_<id>(p)` instead of inlining the condition — the `&Parser` receiver keeps it read-only, so it is callable from every decision path (the token-led / interchangeable-`Ambiguous` dispatch and the ATN seed prune). Context-dependent gates (`$v` → `vals.v`) stay folded into the dispatch, where `vals` is in scope. Fixtures `wado_pred_la.g4` (`{p.la(1) == TK_NUM}?` reading `p`), plus the existing `wado_pred_{select,direct,single}.g4` now routing through `pred_<id>`.
    - [x] The ATN parser predicate path. The ATN respects a gated predicate by **exclusion**: the caller pre-evaluates each gated alt's condition (alt-initial, so its context is in scope at the decision) and passes the false ones to the simulator, which skips seeding them (they can never win). This keeps the simulator a pure function of (ATN, tokens, disabled set) — plain data, unit-testable without a caller receiver — and mirrors the existing precedence-predicate prune.
      - [x] Simulator support: `atn_predict_with_stack` takes a defaulted `disabled_alts: &List<i32>` (`&ATN_NO_DISABLED`) and skips seeding any listed alt at the decision — the alt index is the seed's `alt`, which the dispatch maps back to the grammar alt. Byte-behavior-identical for every current call site (all use the default). Unit test `predict excludes a disabled alternative` (`atn_sim_test.wado`).
      - [x] Codegen wiring: `ambiguous_node_routes_via_atn` (shared by marking and `gen_prediction_code_inner`'s `route_via_atn`, so they cannot disagree) marks an ATN-routed (`AtEndConflict` + `emits_at_end_conflict_via_atn`) group's gated predicates as gated (suppressing the inline copy / unsupported warning), and `emit_atn_disabled_seed` emits each gated alt's `gate_condition_for` condition — `pred_<id>(p)` for a context-free body, `vals.<v>` for a context-dependent one, both in scope at the decision — as `if !(<cond>) { _atn_disabled.push(<alt>); }` before the `atn_predict_with_stack` call. When a seed is present, a `_atn_alt < 0` guard raises no-viable (all-excluded) instead of falling through to the last alt. Byte-identical for gate-free ATN grammars. Fixture `wado_atn_pred.g4`: `{false}?` (`rejects`) and `{$mode == 0}?` (`ctxdrop`) both force the shorter alt on `abcc`; `allfalse` fails no-viable.
- [x] Phase 3 — java2wado for the corpus **parser** subset + members translation, wired into codegen (see the java2wado section above). A non-superClass `language = Java` grammar's parser actions / predicates / `@members` / ctx-cast LR idiom execute during the parse (`driver_java_{action,members,pred}_test`).
  - [x] Stage C output-compare harness: the descriptor extractor emits a `stage_c/<Category>/<Name>_output_test.wado` for every Parser descriptor whose `[output]` is action-print text (rejected by `normalize_output_for_stage_b`), asserting `result.output == [output]` on the generated parser. Landed for `Sets` + `SemPredEvalParser` (41 pass; 6 `[stage_c_todo]` — nested-in-repeat actions, non-ASCII prints, context-dependent-pred divergences). Reuses the Stage A generated parser; `[stage_c_todo]` / `[stage_c_skip]` in `status.toml` triage the gaps. Running the remaining categories is mechanical follow-up.
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
- Whether a Kiln generator option may override `action_language`.
- `$v` semantics after recovery beyond Default-init (is Default always right for user types?).
- Predicate eval-count divergence policy: how much trace mismatch is acceptable before a descriptor is `[skip]` instead of `[todo]`.
- IR details: which element `<p=3>` legally attaches to (confirm via jar); whether upstream accepts `@init` / `@after` on lexer rules.
- `catch` / `finally` execution semantics under Gale's resilient-parse model (no exceptions in Wado) — parked, IR retains them.
- Effect-generic parse functions (user actions with real effects via handlers) — future extension.
- Java numeric promotion: java2wado does not model Java's implicit numeric widening. `token_int` (`$X.int`, `.type`, `.line`, `.pos`, `.index`) is `i32`, so mixing it with a wider value-channel field — `returns [long v]` (`i64`), `[float x]` (`f32`), `[double d]` (`f64`) — mismatches Wado's strict widths in any context (`$v = $X.int`, `$v + $X.int`, `println($v + $X.int)`), since Wado has no implicit widening. No corpus grammar hits this (a wider field is normally paired with a wider source), and the failure is a loud generated-parser type error, not a silent miscompile. A proper fix threads Java's promotion rules through the java2wado emitter (cast at the point the promotion is required); an assignment-only cast would be a partial workaround that leaves the expression cases broken.
- License hygiene: template-helper semantics and any oracle pinning stay jar-black-box only (documented in `src/g4/action_templates.wado`).
- Retention gaps: (a) _(silent drop fixed)_ `empty_alt_group_as_optional` used to fold a predicate-only empty branch `( {p}? | A )` into an Optional, dropping the gate before action ids were assigned — silently, since the body never reached `warn_unhandled_actions`. The fold now bails when the empty alt carries actions, so the group is retained and the predicate surfaces loudly (`UnsupportedAction`); executing it as a real skip gate (the lowering-structure change) is still deferred. This was corpus-live: `ParserExec/PredicatedIfIfElse`'s `('else' stmt | { _input.LA(1) != ELSE }?)` is the exact shape. The enabling root cause — `grammar_has_actions` inspecting only top-level `alt.actions`, so a grammar whose only action sits in a group left `emit_actions` off (the whole action pipeline, including the safety net, never ran) — is fixed too: it now recurses group elements on both the parser and lexer sides. (b) `gen_lexer_non_greedy_repeat` builds a synthesized suffix `LexerAlternative` from an element slice without carrying/re-basing the suffix's `AltAction` `before_index`.

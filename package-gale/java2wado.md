# java2wado — Java action-body translation plan

Plan for Stage C's third layer (see [`action.md`](./action.md) §Translator interface, and its staging §Phase 3): translating the Java subset that appears in ANTLR action bodies to Wado, so `language = Java` grammars execute their actions. Companion to [`action.md`](./action.md) (the action-execution design) and [`TODO.md`](./TODO.md) §"Stage C". Status: plan — not yet implemented.

## Where this sits

The identity translator (`src/action_translate.wado`, `language = Wado`) already does the hard, host-language-independent half:

- **Attribute resolution** (`attr_scan` → `attr_resolve`): finds `$x`, `$x.text`, `$ctx`, `$text`, `$start`/`$stop`, resolves them against the rule's label / arg / returns / locals environment, classifies the target.
- **Value channel**: `$v` / `$local` / `$arg` → `vals.<name>`, cross-rule `$a.v` → `a.v`, token members → `p.token_*`, rule-span specials → `p.text_span` / `p.rule_string_tree`.
- **Substitution engine** (`attr_substitute::splice_attrs`): rewrites the resolved `$`-spans in place.

For `language = Wado` the surrounding host text is already Wado, so splice-in-place is the whole job. **java2wado is exactly that, plus a translation of the surrounding Java host code** — the `$`-spans resolve to the *same* Wado expressions over the *same* runtime API / value channel. Nothing in the attribute/value-channel layer changes; java2wado is a new front end that parses Java and re-emits Wado, using the existing attribute resolver to fill the `$`-leaves.

Single wiring lever: `action_language_is_emittable` (`codegen.wado:208`) currently returns `grammar.action_language matches { Wado }`. Making Java emittable flows `emit_actions = true` through lowering / parser_gen / lexer_gen exactly as Wado does today.

## The corpus subset (measured, 2026-07)

Surveyed from `tests/antlr4-compat/grammars/`. This is the concrete target — the subset the parser must cover, no more.

**Statements**

- Expression statement: `System.out.println(...)`, `this.method(...)`.
- Local declaration: `int x = 0;`, `boolean b = true;` (in `@members`, `@init`, `@after`).
- Assignment incl. compound: `$v = ...;`, `$result = "(" + ... + ")";`, `this.i += 1;`.
- (`assert` appears in `action.md`'s target list but not the descriptor corpus; include only when a fixture needs it.)

**Expressions**

- Literals: string `"..."`, int `0` / `1000`, `true` / `false`.
- Operators: `+ - * / %`, `== != >= <= > <`, `&& || !`. `+` over a string operand is Java string concatenation.
- Method calls (API-mapped, see table): `getText()`, `_input.LA(1)`, `_input.LT(1).getText()`, `getCharPositionInLine()`, `_tokenStartCharPositionInLine`, `x.equals("y")`.
- `this.field` / `this.method(args)`.
- Qualified constant: `TParser.ELSE`, `TParser.NL` → the generated `TokenKind` constant.
- ctx cast in the LR-binary pattern: `((BinaryContext)$ctx).e(0).v` → the LR lhs/rhs vals locals (already handled by the value channel; java2wado only strips the Java cast/child-accessor syntax around it).
- `$`-references anywhere a primary expression is expected — opaque to the Java parser, resolved by `attr_resolve`.

**Predicates** (same expression grammar, boolean-typed)

`{this.i % 2 == 0}?`, `{_input.LA(1) != TParser.ELSE}?`, `{getCharPositionInLine() < 2}?`, `{$i == 1}?`, `{false}?`, `{getText().equals("enum")}?`. `{3 >= $_p}?` needs `$_p` → `min_prec` (an open item in `action.md`).

**Members** (`@parser::members` / `@lexer::members`)

- Field declaration → struct field: `boolean enumKeyword = true;` → `enumKeyword: bool = true`.
- Method declaration → method: `public void foo() { System.out.println("foo"); }` → `fn foo(&mut self) { ... }` with `this.` → `self.`.

## API / type mapping

| Java                              | Wado                                    |
| --------------------------------- | --------------------------------------- |
| `System.out.println(x)` / `print` | `p.emit(...)` (see `action.md` §Effects)|
| `getText()`                       | `p.rule_text()` / lexer `lx.text()`     |
| `_input.LA(k)` / `_input.LT(k)`   | `p.la(k)` / `p.lt(k)`                    |
| `_input.getText()`                | `p.input_text()`                        |
| `.getText()` on a token/LT        | `p.token_text(<idx>)`                    |
| `getCharPositionInLine()`         | `lx.column()`                           |
| `_tokenStartCharPositionInLine`   | `lx.token_start_column()`               |
| `$ctx.toStringTree(this)`         | `p.rule_string_tree()` (already mapped) |
| `x.equals(y)`                     | `x == y`                                 |
| `TParser.<TOKEN>`                 | generated `TK_<TOKEN>` constant         |
| `int` / `boolean` / `String` / `void` | `i32` / `bool` / `String` / `()`    |
| `List<String>`                    | `List<String>`                          |

The recognizer handle is the literal `this`: `this.field` / `this.method()` and the recognizer methods (`getText`, `_input.LA`) all emit onto `this`. The attribute engine is handle-parameterized (`resolve_attr_ref(..., handle)`, default `p` for the live Wado path), so java2wado passes `this` and `$`-refs read `this.token_text(...)` too — one handle across the whole translated body. Codegen binds `this` as a reference to the parser at the splice site (`let this = p;` in a parse / predicate body, `let this = self;` in a `@members` method); Wado method calls auto-deref, so no `*` is needed. This keeps the translator decoupled from codegen's parameter name.

Anything outside the subset is a **loud generation diagnostic** carrying the fragment span — never a silent no-op. The subset grows on corpus demand.

## Design: a real (small) Java parser, not regex rewriting

Rationale: string concatenation (`"a" + $x + "b"`), operator precedence, and `this.`-rewriting cannot be done soundly by span substitution alone (the identity translator's model). java2wado parses Java into a small AST and re-emits Wado. `$`-references are lexed as an opaque primary token whose text is resolved through the existing `attr_resolve` + value-channel logic, so the two translators share one attribute engine.

Module layout mirrors `src/g4/` (lexer / parser split):

- `src/java2wado.wado` — facade + the `ActionLanguage` dispatcher (`translate_action` / `translate_predicate` / `translate_members`).
- `src/java2wado/lexer.wado` — Java tokens over the fragment: identifiers, `$ident` (opaque primary), int/string/bool literals, operators, `.`/`,`/`;`/`()`/`{}`, casts. Reuse the string/comment-skipping discipline in `action_strip` / `attr_scan`.
- `src/java2wado/parser.wado` — recursive descent producing `JExpr` / `JStmt` (variants) for the subset above. `$`-refs and unknown method calls become `JExpr::Attr(span)` / a mapped-call node.
- `src/java2wado/emit.wado` — `JStmt`/`JExpr` → Wado string. Leaves that are `$`-refs call into `action_translate`'s attribute resolution (refactored so java2wado and the identity path share `action_attr_expr`); mapped calls consult the API table; `TParser.X` consults the token-kind table.

Each with a sibling `_test.wado`.

The public surface mirrors `action.md`'s `ActionTranslator` sketch but dispatches by `ActionLanguage` (no dynamic dispatch in Wado yet):

```
translate_action(lang, rule, rules, body, vals_in_scope, bindings) -> Result<String, String>
translate_predicate(...) -> Result<String, String>
translate_members(lang, body) -> Result<MemberDecls, String>
```

`lang == Wado` routes to today's `translate_wado_action`; `lang == Java` routes to java2wado. Callers in `parser_gen` / `lexer_gen` / `cst_gen` switch from calling `translate_wado_action` directly to the dispatcher, passing `grammar.action_language`.

## The `emit_actions` rollout — cover-then-flip, one carve-out

Today a Java grammar has `emit_actions = false`: actions are discarded and the generated parser is **byte-identical to actionless**, so the whole descriptor corpus passes on tree-shape alone. The rollout: **cover the corpus subset first, then flip Java emittable in one step, then fix the fallout** — the fallout is small and quick to fix because the corpus Java surface is small and regular (survey above). No general per-grammar "does everything translate" gate is built; an untranslatable action stays a **loud error**, which is what forces coverage rather than hiding gaps.

**One principled carve-out, not a gate.** Blast-radius survey of what codegen actually runs (driver tests + descriptor tests): the whole descriptor corpus is coverable in Phase 3 (its `this.pred(...)` etc. are `@members`-defined, so members translation reaches them). The single exception is grammars declaring `superClass` — `RustLexer.g4` / `TypeScriptLexer.g4` are in driver tests, and their actions (`{this.SOF()}?`, `{this.ProcessOpenBrace();}`) call a hand-written base class that lives **outside** the `.g4`. Gale cannot see that body, so translating it is not a java2wado coverage gap — it is Phase 4's SuperClass-trait job by definition. Carve it out with one semantic check:

```
action_language_is_emittable(g)
    = g.action_language matches { Wado }
   || (g.action_language matches { Java } && !grammar_has_superclass(g))
```

This is one boolean, not fragment-scanning machinery; it is principled (superClass = external base = Phase 4) and permanent (Phase 4 turns it into "emit via the trait"), not throwaway scaffolding. Non-superClass Java grammars — the entire descriptor corpus — flip unconditionally; any untranslatable fragment there is a loud error and gets fixed on the spot.

Distinct from a miscompile: a discarded action under the superClass carve-out is the current *documented* behavior; an untranslatable ref *inside* an emitting grammar stays a loud generation error.

## Staged, TDD

Each step is red/green: a failing `java2wado_test.wado` case (or a driver `[output]` fixture) first, then the code.

- **3.i — scaffold + dispatcher + carve-out.** New `src/java2wado.wado` facade; `translate_action`/`_predicate`/`_members` dispatcher on `ActionLanguage`; add the `superClass` carve-out to `action_language_is_emittable` (`!grammar_has_superclass(g)`). Java stays **non-emittable** until 3.vii flips it, so no behavior change yet — this step only proves the dispatcher routes Wado unchanged and the carve-out predicate is correct (RustLexer/TypeScriptLexer excluded, a plain-Java grammar included).
- **3.ii — expression core.** Java lexer + expression parser + emitter for literals, arithmetic/comparison/logical/`%`, string concatenation, `$`-leaves via shared `action_attr_expr`. Covers `{$i == 1}?`, `{this.i % 2 == 0}?` (needs `this.field`), `$v = $x.v+1;`. Fixtures per operator class.
- **3.iii — statements.** Expression statements, local declarations, assignments incl. compound. `System.out.println/print` → `p.emit`. `getText()` → `p.rule_text()`. Unblocks the print-`[output]` descriptor family (`{System.out.println($e.v);}`, `{System.out.print($a.text);}`).
- **3.iv — mapped calls + qualified constants.** `_input.LA/LT`, `getCharPositionInLine`, `_tokenStartCharPositionInLine`, `x.equals(y)`, `TParser.X` → `TK_X`. Unblocks `{_input.LA(1) != TParser.ELSE}?`, `{getCharPositionInLine() < 2}?`, `{getText().equals("enum")}?`.
- **3.v — ctx-cast LR-binary.** Strip `((BinaryContext)$ctx).e(0).v` down to the LR vals locals the value channel already exposes. Fixture mirrors the corpus LR-binary grammar.
- **3.vi — members.** `translate_members`: field decls → struct fields (translated initializers), method decls → methods (`this.` → `self.`). Wire into the generated `Parser` / `Lexer` struct emit. Unblocks `@parser::members {boolean enumKeyword = true;}` and `{int i = 0;}`, i.e. the `SemPredEvalParser` member-backed predicates.
- **3.vii — flip + corpus sweep + acceptance.** Flip `action_language_is_emittable` to accept non-superClass Java (the cover-then-flip step). Re-triage the descriptor corpus: print-`[output]` and `SemPredEval*` descriptors lose their auto-`#[TODO]` and compare `result.output`; fix any fragment that errors (small, by construction). Confirm RustLexer/TypeScriptLexer driver tests stay byte-identical (carve-out holds). Update `action.md` staging, `TODO.md` §Stage C, `antlr4-compatibility.md`.

Out of scope for Phase 3 (tracked elsewhere): `superClass` trait + real-world Rust/TypeScript grammars (Phase 4); lexer actions / position-sensitive lexer predicates beyond what Phase 2 emits (Phase 4); `catch`/`finally` (parked, no Wado exceptions); `$_p` in Java predicates (open item, maps to `min_prec`); `@header` Java imports (warn-and-drop).

## Open questions

- Java string concatenation target: `+` over Wado `String` — does the value channel expose `String` `+`, or must java2wado emit a template literal? Decide when 3.ii lands the first `"a" + $x` fixture.
- Method-decl bodies in `@members` recurse into the same statement translator — scope: only the subset, or a loud error on anything richer? (`DelegatorAccessesDelegateMembers` `public void foo()` is trivial; keep the parser minimal.)
- Whether a Kiln generator option may force `action_language` (already open in `action.md`).

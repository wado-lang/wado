# Grammar Imports (`import S;`)

`import` is parsed and discarded: `parse_delegate_grammars` advances past the names and records nothing. Every `CompositeLexers` / `CompositeParsers` descriptor is therefore held at parse-only, and a Wado dialect grammar cannot build on `Wado.g4` without forking it ([WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md)).

This is the design that closes it. The ANTLR4 semantics below come from the descriptor corpus — each rule names the descriptor that pins it — and the unpinned edges are marked as oracle questions rather than guessed.

## Resolution is by grammar name, against the declared inputs

ANTLR4 resolves `import S;` to an `S.g4` found on the `-lib` path. A Kiln generator has no filesystem: every input arrives by value, listed at the `use` site, and that list is part of the cache key. So Gale resolves an import against the inputs it was handed, keyed by the name each one _declares_ in its `grammar` header — never by filename.

The corpus forces the same conclusion independently. `DelegatorInvokesFirstVersionOfDelegateRule` writes `import S,T;` while the extractor names the files `.slave1` (grammar T) and `.slave2` (grammar S): import order is the reverse of file order, and only the declared name resolves both.

An import naming no supplied input is an error — `unresolved import "S": add its .g4 to the generator's inputs`. Composing without it would surface later as a reference-to-undefined-rule error pointing at the wrong file.

`import Foo = Bar;` already parses. `Bar` is the grammar name and is what resolves; the alias is recorded and unused until the oracle says what it means.

## Two merges, not one

`merge_grammars` concatenates every input unconditionally. That is right for a split grammar (`RustLexer.g4` + `RustParser.g4` — one grammar in two files) and wrong for a delegate.

|                 | Split halves                               | Delegates                                               |
| --------------- | ------------------------------------------ | ------------------------------------------------------- |
| Same-named rule | duplicate parser rule → error              | the master's wins                                       |
| Rule order      | input order                                | master's first, then import order                       |
| Composite name  | common name, `Lexer` / `Parser` suffix cut | the master's, unchanged                                 |
| Composite kind  | `Combined`                                 | the master's, promoted only when the other kind arrives |

Partition in one pass: take the union of every input's import list; an input whose declared name is in that set is a delegate, and everything else is a split half. The primary is always a master. A grammar with no `import` anywhere keeps today's path byte-identical — the compatibility hinge, since the existing corpus must not move.

`assemble_grammar` then runs parse → partition → `merge_grammars` over the master set → compose the delegates → `finish_grammar`. Both surfaces, the Kiln generator and `gale gen`, already funnel through it.

## Composition

Delegate order is a depth-first pre-order walk of the import closure from the master, first occurrence winning. One level is pinned by `DelegatorInvokesFirstVersionOfDelegateRule`; the corpus has no transitive import, so deeper order is an oracle question. A cycle terminates on the visited set and is reported, not silently composed.

Walking the master and then each delegate in that order:

1. **A rule name defined earlier wins.** The master overrides every delegate, and an earlier delegate a later one. Pinned for parser rules by `DelegatorRuleOverridesDelegate` and `DelegatorRuleOverridesDelegates`, for lexer rules by `LexerDelegatorRuleOverridesDelegate`.
2. **A dropped rule takes its pending refs and its implicit literals with it.** `DelegatorRuleOverridesDelegate`'s slave defines `b : B` and the master overrides `b`; nothing else defines `B`, so a ref surviving the dropped body fails `check_references`.
3. **Rule order is master-first, then delegate order.** Lexer precedence _is_ rule order: `KeywordVSIDOrder` needs the master's `A : 'abc'` to beat the imported `ID : 'a'..'z'+`. It also keeps the composite's start rule the master's first rule.
4. **References resolve after composition, across the whole composite.** `DelegatorRuleOverridesLookaheadInDelegate` overrides `type_` in the master, and the delegate's `decl` — which calls `type_` — must reach the override.
5. **One token space: a `tokens{}` entry yields to a real rule anywhere in the composite, and to an earlier virtual one.** `DelegatesSeeSameTokenType` declares `A` in two delegates' `tokens{}` blocks and defines `A` as a lexer rule in the master. The virtual-token dedup in `parse_grammar` is per-file, so a plain concatenation emits three rules named `A`.
6. **Grammar-level named actions concatenate.** `DelegatorAccessesDelegateMembers` calls `foo()`, declared in the delegate's `@parser::members`.
7. **Rule-attached actions, args and returns travel with the rule.** `ImportedRuleWithAction` (`@after`), `DelegatorInvokesDelegateRuleWithArgs` (`a[int x] returns [int y]`).
8. **Modes and channels merge with index remapping.** No descriptor covers it; `merge_grammars` already does it, and the dialect consumer needs it — `Wado.g4` carries a `TEMPLATE` mode, so a dialect importing it inherits a mode table.
9. **Options are not imported.** `ImportedGrammarWithEmptyOptions` pins only that an empty block does not break the merge, so the general rule — and with it `superClass`, which is read out of `options` — is an oracle question.
10. **Provenance concatenates.** Every delegate path joins `source_files`, so the generated header's `sources = [...]` names what actually built the parser.

## Diagnostics

`parse_delegate_grammars` must record each import's span. An unresolved import then points at the `import` clause instead of at the grammar as a whole — Gale's first diagnostic with a natural span. `SourceSpan` is byte-based where a g4 span is a char offset into `List<char>`, so that conversion is what a spanned import diagnostic costs; a spanless message naming the grammar is the fallback, and matches every Gale diagnostic today.

## What it does not give

- **No rule extension.** An ANTLR4 override replaces a rule; there is no "add an alternative". A dialect adding markup to an expression rule restates that rule in full and owns the copy.
- **No cross-package input.** Kiln `inputs` are project-relative paths (`InvocationPath`), so a dialect in its own repository still needs a copy of `Wado.g4` in its tree. Import resolution shrinks that from a fork that drifts to a copy a checksum can hold; the remainder is a Kiln gap, not a Gale one.

## Corpus consequences

The composite exclusion is categorical: three `slave_grammars.len() == 0` conjuncts in the Stage A eligibility rules and one `continue` ahead of Stage B / Stage B′, all in `scripts/extract_antlr4_descriptors.wado`. Dropping them puts the 17 composite descriptors under the same eligibility rules as every other descriptor, with the emitted `use ... with { generator: ... }` gaining `inputs: ["../../grammars/<Category>/<Name>.slaveN.g4"]`. How many then emit is what the first re-extract reports.

Two limits survive. The composite grammars report through action prints rather than trees, so Stage B — which compares the descriptor's own `[output]` — stays out of reach, and the two `CompositeLexers` descriptors wait on Stage C. Stage B′ takes its expected tree from the jar instead of from `[output]` and so has no such problem, but it needs `antlr4-oracle.sh` to put the slaves where the jar looks for them; today it copies a single `.g4` into its temp directory.

## See Also

- [`antlr4-compatibility.md`](./antlr4-compatibility.md) — the compatibility contract and the descriptor pipeline these consequences land in.
- [`TODO.md`](./TODO.md) — the open entry this design covers.
- [WEP: Markup Dialect](../docs/wep-2026-08-29-markup-dialect.md) — the consumer outside the corpus.

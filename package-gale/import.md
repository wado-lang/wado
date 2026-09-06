# Gale — Grammar Composition (`import S;`)

How a master `.g4` and the grammars its `import` names become one grammar. Companion to [`antlr4-compatibility.md`](./antlr4-compatibility.md) and [`TODO.md`](./TODO.md). The semantics are ANTLR4's, from `vendor/antlr4/doc/grammars.md` ("Grammar Imports"). Only the resolution model is Gale's own, because a Kiln generator has no filesystem.

## Resolution by declared name, over supplied inputs

ANTLR4 looks an import up by filename under `-lib`. A Kiln generator reads nothing: its inputs arrive by value, so the file set has to be known before it runs. The grammars a composition may reach are therefore exactly the ones the `use` clause hands it:

```wado
use { Parser } from "./M.g4" with {
    generator: { module: "wado-lang:gale@0.1", inputs: ["./S.g4", "./T.g4"] },
};
```

`import S;` binds to whichever supplied input declares `grammar S` in its header — never to a filename. The corpus forces this: `DelegatorInvokesFirstVersionOfDelegateRule` writes `import S,T;` where the extracted files are `.slave1` (grammar `T`) and `.slave2` (grammar `S`).

A name therefore has to identify exactly one input, and both ways of failing that are loud. An import naming no supplied input is an error rather than a silent omission, and two inputs declaring one name are rejected before anything is composed — otherwise the second is neither merged nor folded, and vanishes.

Two composition rules run over the same input list, and `compose_grammars` partitions in one pass:

- A grammar whose declared name appears in some input's import list is a **delegate**.
- Everything else is a **split half** of the master (`FooLexer.g4` + `FooParser.g4`), concatenated by `merge_grammars` exactly as before. A grammar with no `import` anywhere in the set takes that path untouched.

The composite keeps the master's name, kind, options and `superClass`.

## Order: depth-first, first version wins

Delegates are visited depth-first in import order, each on its first visit. That order decides which version of a rule survives. `doc/grammars.md`: a `Nested` importing `G1, G2` where `G1` imports `G3` is examined as `Nested`, `G1`, `G3`, `G2`.

Master rules come first in the composite, then delegates in that order. Two things ride on it:

- Lexer precedence is rule order, so the master's `A : 'abc'` beats an imported `ID : 'a'..'z'+` (`KeywordVSIDOrder`).
- Token types are assigned in rule order, which is what `LexerDelegatorInvokesDelegateRule` pins: the master's `B`/`WS` take 1 and 2, the delegate's `A`/`C` take 3 and 4.

## What merges

| Carried over from a delegate             | Rule                                                                                         |
| ---------------------------------------- | -------------------------------------------------------------------------------------------- |
| Parser and lexer rules                   | Only where the name is still free; a name already defined is dropped                         |
| `tokens { }` entries                     | One token space: a placeholder yields its slot to a real rule from anywhere in the composite |
| Modes and channels                       | Merged, with the delegate's indices remapped                                                 |
| Named actions (`@members`)               | Concatenated, each tagged with the recognizer that declared it                               |
| Options (incl. `superClass`, `language`) | Not inherited — "ANTLR also ignores any options in imported grammars"                        |

`language` follows the other options, so the master alone picks the action translator. A composite holds one action language today. A delegate that declares a different one has its bodies translated as the master's, which the translator reports rather than mistranslates. Letting a delegate bring its own is queued in [`TODO.md`](./TODO.md).

An overridden rule takes its own references with it. `DelegatorRuleOverridesDelegate` drops a delegate's `b : B` that is the only reference to `B`. Keeping that reference would fail `check_references`, which runs once over the whole composite.

A mode that an override emptied is discarded, and the remaining modes are renumbered. A mode a surviving `pushMode` / `mode` command targets is kept, or the command would dangle.

## What is rejected

`import Foo = Bar;`. ANTLR4's aliased form names the delegate for qualified action references (`gFoo.x`), and Gale has no counterpart for those. Accepting it while dropping what the alias means would be worse than refusing it, so it is a loud error for now. Closing that is queued in [`TODO.md`](./TODO.md); it is the one place Gale rejects a grammar ANTLR4 compiles.

## Embedding a language

Composition also nests one language inside another, which is what
`package-gale/example/` demonstrates. Nothing beyond the rules above carries
it, given two conventions in the delegate:

- Its lexer rules live in a mode of its own (`mode CSS;`), so they are
  unreachable until the host pushes into it. A composite has one lexer, and
  without a mode the host's catch-all text rule swallows the embedded body.
- Its token names are prefixed. One token space means the first rule of a given
  name wins, so `CSS_IDENT` and `JS_IDENT` must differ to both survive.
- Its parser rules name their tokens rather than spelling them: `CSS_COLON`,
  not `':'`. A parser literal aliases only to a `DEFAULT_MODE` rule, and mints
  an anonymous `DEFAULT_MODE` token where none matches. The lexer, sitting in
  the delegate's mode, never produces that token. The gate is deliberate: a
  parser rule is reached in whatever mode its caller pushed, and nothing records
  that, so a literal is read as the mode the parser starts in.

The host declares the same mode name for the token that leaves it
(`STYLE_CLOSE : '</style>' -> popMode ;` under its own `mode CSS;`), and the
two declarations unify by name. The host therefore owns both boundaries and the
delegate names no host.

## Known gaps

- Stage B′ does not oracle a composite: `antlr4-oracle.sh` invokes the jar on one grammar file with no `-lib` slave lookup. Composite descriptors are pinned by Stage A and Stage C only.
- An override replaces a rule. There is no "add an alternative", so a dialect restates any rule it extends.
- A delegate written to be embedded cannot also be used on its own: the mode its rules sit in is decided by the file declaring them, so standalone it starts in an empty `DEFAULT_MODE`. Closing it means the host saying where a delegate's rules land; see [`TODO.md`](./TODO.md).
- Kiln `inputs` are relative paths inside the project, so a dialect living in its own repository still holds a copy of the grammar it extends. Resolution shrinks that from a drifting fork to a copy a checksum can hold. Reaching a grammar inside a dependency package is a Kiln gap.

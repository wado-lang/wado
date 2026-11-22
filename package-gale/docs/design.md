# Gale Design

Behavioral ANTLR4 compatibility, with a small superset. The goal is not just to
_accept_ the grammars ANTLR4 accepts, but to _parse like ANTLR4 does_ — same
precedence, same ambiguity resolution, same parse trees. A `.g4` that the
upstream `antlr4` tool accepts should parse identically through Gale. Gale also
accepts a few grammars ANTLR4 _rejects_, but only where the meaning is fixed
with no remaining choice — e.g. a lexer `mode` inside a combined `grammar`,
which ANTLR4 restricts to a `lexer grammar` but which is unambiguous since a
combined grammar already bundles a lexer. [`antlr4-compatibility.md`](./antlr4-compatibility.md#grammars-gale-accepts-and-antlr4-rejects)
lists them all. Where
the meaning is not uniquely determined, Gale rejects loudly rather than guessing.
That costs it one grammar ANTLR4 accepts, `import Foo = Bar;`, carved out under
claim (a) in [`antlr4-compatibility.md`](./antlr4-compatibility.md).

Self-contained output, no version drift. Gale inlines its entire runtime into
every generated parser. There is no `gale-runtime` package to keep aligned with
the generator: each generated file carries the exact runtime it was built with,
and regenerating upgrades it.

Adaptive LL with a runtime ATN simulator. Most decisions are resolved with
fast static lookahead. When a decision genuinely needs unbounded,
full-context lookahead — the ALL(\*) cases that defeat any fixed-`k` LL parser
— Gale emits a runtime ATN simulator for exactly those decisions and leaves
the rest of the parser on the fast static path. (See the ALL(\*) example in
the [README](../README.md#beyond-ll-an-all-example).)

Error-resilient parsing. A generated parser never bails on a syntax error. It
always returns a tree plus a list of diagnostics, recovering locally and
recording every edit, so a broken input still yields a usable tree (handy for
editors, linters, and language servers). A clean parse simply has an empty
diagnostic list. [`resilient-parser.md`](./resilient-parser.md) holds the
design.

Built-in diagnostics and debugging. `gale dump` shows the prediction decision
for every parser rule (`--atn` shows the simulator's automaton, `--lexer` the
emit strategy of every lexer rule); the `trace` option makes a generated parser
log its recursive descent through `core:log`. See
[The `gale` command](../README.md#the-gale-command) and
[`debugging.md`](./debugging.md).

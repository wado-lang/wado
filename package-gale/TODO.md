# Gale TODO

Known gaps to close on the road to **full ANTLR4 compatibility**. The
compatibility principle is documented in [AGENTS.md](./AGENTS.md): the g4
parser must accept any well-formed grammar that upstream `antlr4` accepts,
with the single exception that host-language action bodies are skipped.

The g4 parser already accepts the full ANTLR4 surface syntax (with that
exception). The remaining work is mostly about **propagating** parsed
information into the IR and **using** it in the code generator so that
generated parsers are semantically correct, not just syntactically accepted.

## Generated Parser Bugs

(none currently)

## Future Work: Actions and `superClass` (low priority)

Gale currently skips the contents of `{ ... }` action blocks and semantic
predicates — the g4 parser recognizes them (so real-world grammars parse
cleanly) but the code generator discards the host-language source. This is
intentional for the near term: emitting Wado from opaque Java/Rust/Python
snippets requires a cross-language translator we do not have.

Once action-body support is designed, `Grammar.options.superClass` becomes
meaningful and can be wired through as a trait bound on the generated
parser/lexer struct (something like `impl SuperClass for GeneratedParser`,
with action bodies able to call `self.helper(...)`). Until then the option
is surfaced only as a metadata comment.

Rough sketch, for when this is picked up:

- Extend the IR so `OptionValue::Action` and per-alt action elements carry
  a language-tagged source fragment instead of being a placeholder string.
- Add a pluggable "action translator" interface; ship at minimum an
  identity translator for Wado-written action bodies.
- Generate a `SuperClass` trait (name derived from `superClass = Foo`) and
  require callers to `impl` it; emit action bodies as method calls on
  `self` that resolve through that trait.
- `tokenVocab` falls out naturally at that point — another grammar's
  generated `TokenKind` enum can be imported by name rather than merged at
  IR time.

No work here blocks any current Gale user. Re-prioritize only when a real
grammar outside the `clean` set (ANTLR4, Rust, TypeScript lexers) needs its
action semantics reproduced, not just skipped.

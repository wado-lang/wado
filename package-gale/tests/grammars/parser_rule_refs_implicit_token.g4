// Min repro: parser rule references an uppercase identifier (`K_IN`)
// that is never defined as a lexer rule. ANTLR4 treats such a name as
// an implicit token to be synthesized (see vendor/antlr4 docs); Gale's
// post-merge `check_references` rejects the grammar outright.
//
// Today: `wado run package-gale -- gen ...` exits before codegen with
//   gale: reference to undefined token "K_IN" in "expr"
//
// Covers stage_a_skip entries (status.toml reason "all-binary-op LR
// rewrite bug" is wrong; both grammars compile cleanly once the
// implicit-token references are turned into explicit lexer rules —
// verified by stripping K_* refs from
// LeftRecursion/WhitespaceInfluence_1.g4 and re-running Gale):
//   - LeftRecursion/WhitespaceInfluence_1
//   - LeftRecursion/WhitespaceInfluence_2
//
// Fix direction (in priority order):
//   1. ANTLR4-compatible: synthesize an implicit lexer rule for any
//      uppercase name referenced from a parser rule but never defined.
//      Matches `vendor/antlr4/doc/lexer-rules.md` semantics.
//   2. Stricter alternative: keep rejecting, but only after surfacing
//      the same diagnostic ANTLR4 itself prints for this shape.

grammar ParserRefImplicitToken;

prog : expr EOF;
expr : ID K_IN ID;

ID : [a-z]+;
WS : [ \t\n\r]+ -> skip;

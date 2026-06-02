// Source: derived from antlr4 runtime-testsuite
//   ParserExec/IfIfElseNonGreedyBinding1
// License: BSD-3-Clause (ANTLR4 upstream)
//
// Regression for non-greedy `(e)??` prefer-skip dispatch.
// Input `if y if y x else x` should bind the `else` to the OUTER
// ifStatement (not the inner one), per ANTLR4's non-greedy `??`
// semantics. Tracked in package-gale/TODO.md "ATN-class grammars"
// and status.toml as `[stage_a_todo] ParserExec/IfIfElseNonGreedyBinding1`.

grammar OptionalNonGreedy;

start : statement+ ;
statement : 'x' | ifStatement;
ifStatement : 'if' 'y' statement ('else' statement)?? ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> channel(HIDDEN);

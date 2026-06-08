// Source: derived from antlr4 runtime-testsuite
//   ParserExec/IfIfElseNonGreedyBinding2
// License: BSD-3-Clause (ANTLR4 upstream)
//
// Regression for the empty-alt-first group `( | 'else' statement)`,
// which is the EBNF equivalent of the non-greedy optional
// `('else' statement)??`: the empty alternative comes first, so the
// decision prefers to skip. Input `if y if y x else x` must bind the
// `else` to the OUTER ifStatement (same as Binding1). Tracked in
// package-gale/TODO.md "ATN-class grammars" and status.toml as
// `ParserExec/IfIfElseNonGreedyBinding2`.

grammar EmptyAltGroup;

start : statement+ ;
statement : 'x' | ifStatement;
ifStatement : 'if' 'y' statement (|'else' statement) ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> channel(HIDDEN);

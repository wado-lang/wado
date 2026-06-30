// Source: derived from antlr4 runtime-testsuite
//   ParserExec/IfIfElseNonGreedyBinding1 (extended to two `??` per rule)
// License: BSD-3-Clause (ANTLR4 upstream)
//
// `statement` has two `??` (`('else' …)??` / `('otherwise' …)??`); each must
// predict its own decision so the dangling clause binds to the OUTER statement.
grammar LlOptionalNonGreedyMulti;

start : statement+ ;
statement
    : 'if' 'y' statement ('else' statement)??
    | 'when' 'y' statement ('otherwise' statement)??
    | 'x'
    ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> channel(HIDDEN);

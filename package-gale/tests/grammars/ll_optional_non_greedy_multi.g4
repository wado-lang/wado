// Source: derived from antlr4 runtime-testsuite
//   ParserExec/IfIfElseNonGreedyBinding1 (extended to two `??` per rule)
// License: BSD-3-Clause (ANTLR4 upstream)
//
// Regression for MULTIPLE non-greedy `(e)??` per rule. `statement` carries two
// exit-first decisions (`('else' …)??` and `('otherwise' …)??`). The retired
// `atn_ng_optional_enter` heuristic searched for the rule's UNIQUE exit-first
// BlockStart and, finding two, fell back to greedy ENTER — wrongly binding the
// dangling `else` / `otherwise` to the INNER statement. With the compile-time
// decision-number correspondence each `??` predicts its own decision, so the
// non-greedy prefer-skip semantics bind to the OUTER statement again.
// Tracked in package-gale/TODO.md "ATN-class grammars".

grammar LlOptionalNonGreedyMulti;

start : statement+ ;
statement
    : 'if' 'y' statement ('else' statement)??
    | 'when' 'y' statement ('otherwise' statement)??
    | 'x'
    ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> channel(HIDDEN);

// LL(*) regression: tail-greedy Optional that must yield to a required
// follow sibling.
//
// Source: derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/PredictionMode_LL.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.
//
// Pattern: a rule call site has a *strictly required* next sibling whose
// FIRST set overlaps the callee's tail-greedy first set. ANTLR4 LL
// prediction picks the longer match (`a b`); SLL with greedy `Y?` picks
// the shorter match (`a`) and leaves `Y` unconsumed. Gale's
// `__follow_<id>` variant emit must match ANTLR4's behaviour here.
//
//   Input "X Y":
//     ANTLR4 LL  → (r (a X) (b Y))
//     SLL/greedy → (r (a X Y))      ← Gale must NOT do this

grammar LLBasic;

r : (a b | a) EOF ;
a : X Y? ;
b : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;

// Source: hand-written regression for the FOLLOW-disjoint at-end-conflict
// refinement — the nullable-rule-reference suffix gap.
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// x has an at-end conflict ('a' 'b' ends vs 'a' 'b' 'c' continues on 'c').
// The caller `s : x nb 'c'` presents 'c' after x through the *nullable* rule
// nb (which can match empty), so 'c' IS in FOLLOW(x) and the conflict is
// genuinely context-dependent: on `a b c` (nb empty) x must yield 'c' to s.
// FOLLOW(x) must flow through the nullable RuleRef `nb`, or the routing
// wrongly keeps the static tournament and the parser rejects the valid
// input `a b c`.
grammar LlAtEndNullableGap;

s  : x nb 'c' ;
x  : 'a' 'b'
   | 'a' 'b' 'c'
   ;
nb : 'w'? ;
WS : ' ' -> skip ;

// Source: hand-written, pinning how the tournament breaks an EOF tie.
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// Both alternatives match the same single token. The scan counts `t`'s EOF
// as one more token than the parse consumes, so `t` wins the tie — which is
// the answer the jar gives. Changing the scan to match the parse here would
// hand the tie to `u` and diverge.
grammar LlEofTie;

s : u | t ;
u : A ;
t : A EOF ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;

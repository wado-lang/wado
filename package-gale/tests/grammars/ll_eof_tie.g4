// Source: hand-written regression for the scan/parse EOF asymmetry.
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// Both alternatives match the same single token, so the longest-match
// tournament ties and alternative order decides. `t` reaches EOF, which the
// parse matches without advancing — a scan that advances there reports one
// token more and steals the tie from `u`.
grammar LlEofTie;

s : u | t ;
u : A ;
t : A EOF ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;

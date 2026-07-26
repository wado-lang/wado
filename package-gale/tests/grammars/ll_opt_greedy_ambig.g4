// Source: distilled from SQLite.g4's `column_def : column_name type_name? column_constraint*`.
// License: same as the Gale package.
//
// An ambiguous greedy optional: FIRST(t) and FIRST(c) share NULL, so `x null`
// parses either as `t=null` (enter the optional) or `c=null` (skip it, let the
// loop take it). ANTLR4 resolves the ambiguity by alternative order — entering
// the subrule is alternative 1 — so the optional wins whenever entering leaves
// a viable parse, which is why `x null` binds NULL to `t`. Gale yields the
// shared token to the continuation instead; the driver test pins that
// divergence as `#[TODO]`.
grammar LlOptGreedyAmbig;

s : ID t? c* EOF ;
t : NULL | ID ;
c : NULL ;

NULL : 'null' ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

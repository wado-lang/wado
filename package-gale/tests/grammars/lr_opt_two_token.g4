// Source: distilled from SQLite.g4's `expr … K_IN ( … | ( database_name '.' )? table_name )`.
// License: same as the Gale package.
//
// A two-token optional (`( d '.' )? t` where FIRST(d) == FIRST(t)) inside a
// left-recursive alternative's suffix. Deciding it needs the second token, the
// `.`, so the op-only walker that drives LR-suffix bodies has to honour the
// lowered Optional strategy: a one-token first-set check enters on the shared
// first token and then fails on the missing `'.'`.
grammar LrOptTwoToken;

s : e EOF ;
e : ID
  | e 'in' ( '(' ID ')' | ( d '.' )? t )
  ;
d : ID ;
t : ID ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

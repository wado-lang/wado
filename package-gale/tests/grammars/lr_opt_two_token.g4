// Source: distilled from SQLite.g4's `expr … K_IN ( … | ( database_name '.' )? table_name )`.
// License: same as the Gale package.
//
// A two-token optional (`( d '.' )? t` where FIRST(d) == FIRST(t)) inside a
// left-recursive alternative's suffix. Deciding it needs the second token (the
// `.`). The op-only emit walker that drives LR-suffix bodies used to ignore the
// lowered Optional strategy, so the optional was entered on the shared first
// token and then failed on the missing `'.'` — while the same optional in a
// non-LR rule body committed correctly via its scan guard.
grammar LrOptTwoToken;

s : e EOF ;
e : ID
  | e 'in' ( '(' ID ')' | ( d '.' )? t )
  ;
d : ID ;
t : ID ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

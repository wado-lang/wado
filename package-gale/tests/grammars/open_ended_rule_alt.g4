// Source: hand-written regression grammar for a rule-level open-ended
// alternative.
// License: same as the Gale package.
//
// `A? . C` reaches a wildcard behind a nullable prefix, so it has a first set
// (`a`) and still admits every token. An arm built from it therefore tests
// nothing — and may only keep its place because the partition has merged the
// other alternatives into it. Partitioning the rule by raw first sets left it
// in a branch of its own, so the unconditional arm came first and `b d` /
// `b e` were dead code behind it.
//
//   r : A? . C | B D | B E ;   on `a c`, `b c`, `b d`, `b e`
grammar OpenEndedRuleAlt;

r : A? . C | B D | B E ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
E : 'e' ;
WS : [ \t\r\n]+ -> skip ;

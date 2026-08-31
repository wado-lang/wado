// Source: hand-written regression grammar for a rule-level open-ended
// alternative.
// License: same as the Gale package.
//
// `A? . C` reaches a wildcard behind a nullable prefix, so it has a first set
// (`a`) and still admits every token. An arm built from it therefore tests
// nothing, and may keep its place only where the partition has merged the
// other alternatives into it — which rule level, partitioning by raw first
// sets, does not (see `rule_overlap_groups`).
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

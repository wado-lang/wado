// A rule-level open-ended alternative.
//
// `A? . C` reaches a wildcard behind a nullable prefix, so it has a first set
// (`a`) and still admits every token: the rule's decision has to offer it on
// `b` beside the two alternatives that name `b`, and on every token no
// alternative names. Its `A?` contests every token with the `.` after it.
//
// Measured against the published jar:
//
//   `a b c` → `(r a b c)`   alt 0, selected by the `a` its prefix names
//   `b d`   → `(r b d)`     alt 1
//   `b e`   → `(r b e)`     alt 2
//   `b c`   → `(r b c)`     alt 0, selected by a token its prefix does not name
//   `a c`   → `(r a c)`     alt 0, with `A?` skipped so `.` takes the `a`
grammar OpenEndedRuleAlt;

r : A? . C | B D | B E ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
E : 'e' ;
WS : [ \t\r\n]+ -> skip ;

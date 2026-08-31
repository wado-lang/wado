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
// Measured against the published jar:
//
//   `a b c` → `(r a b c)`   alt 0, selected by the `a` its prefix names
//   `b d`   → `(r b d)`     alt 1
//   `b e`   → `(r b e)`     alt 2
//   `b c`   → `(r b c)`     alt 0, selected by a token its prefix does not name
//   `a c`   → `(r a c)`     alt 0, with `A?` skipped so `.` takes the `a`
//
// The last two are where Gale's static prediction and the jar part company;
// `driver_cst_open_ended_rule_alt_test.wado` marks them `#[TODO]`.
grammar OpenEndedRuleAlt;

r : A? . C | B D | B E ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
E : 'e' ;
WS : [ \t\r\n]+ -> skip ;

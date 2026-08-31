// Source: hand-written regression grammar for Gale's empty alternative.
// License: same as the Gale package.
//
// An empty alternative in a group is not a case the lookahead selects — it
// has no first set at all. What admits it is what may FOLLOW the group, and
// its alternative index is what ranks it against the alternatives that do
// have one. Measured against the published jar:
//
//   'k' ( A | | B ) EOF   on `k b`  → the empty alt cannot be followed by
//                                     `b`, so ANTLR4 takes `B`.
//   'm' ( A | | B ) x EOF on `b`    → `x` can match `b`, so the empty alt is
//                                     viable and its lower index beats `B`.
//   'm' ( A | | B ) x EOF on `a`    → `A` has a first set that matches and a
//                                     lower index still, so it beats skipping.
grammar EmptyAltMid;

s : 'k' ( A | | B ) EOF
  | 'm' ( A | | B ) x EOF
  ;

x : A | B | ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

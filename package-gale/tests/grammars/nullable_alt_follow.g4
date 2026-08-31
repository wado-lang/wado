// Source: hand-written regression grammar for Gale's nullable alternative.
// License: same as the Gale package.
//
// A nullable alternative is selected by two things at once: its own first
// set, and — because it can match nothing — whatever may follow the group.
// `A?` is not the empty alternative; it is an alternative that has a first
// set AND can be skipped. Measured against the published jar:
//
//   'k' ( A? | B ) C EOF   on `k a c` → `a` selects alt 0 by its first set.
//                          on `k c`   → nothing selects alt 0 by first set,
//                                       but it matches empty and `C` follows.
//                          on `k b c` → `b` selects alt 1.
//
// The indirect spelling behaves identically: `x : A? ;` is nullable through
// the rule, which is why the two must not be told apart by counting elements.
grammar NullableAltFollow;

s : 'k' ( A? | B ) C EOF
  | 'm' ( x | B ) C EOF
  ;

x : A? ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

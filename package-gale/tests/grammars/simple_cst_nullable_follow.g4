// Source: hand-written regression grammar for a nullable alternative in a
// SimpleCst group.
// License: same as the Gale package.
//
// `( x | y )` is one distinct rule call per alternative, so it lowers to the
// SimpleCst shape. `x` is nullable through the rule it calls, so what selects
// it is its own first set AND what follows the group — the same rule a
// General group's nullable alternative gets. Measured against the jar:
//
//   `a c` → `(s (x a) c)`   alt 0, selected by its own first set
//   `b c` → `(s (y b) c)`   alt 1
//   `c`   → `(s x c)`       alt 0 matches empty, and `C` follows
grammar SimpleCstNullableFollow;

s : ( x | y ) C EOF ;

x : A? ;
y : B ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

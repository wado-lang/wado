// Source: hand-written regression grammar for a nullable alternative behind a
// transparent group.
// License: same as the Gale package.
//
// `( ( A? | B ) )` wraps a group in a group of one alternative of one element,
// which lowers to the Transparent shape and adds nothing between the inner
// group and the outer alternative. The inner group's nullable alternative is
// therefore selected by what follows the outer one, exactly as it is without
// the wrapper. Measured against the jar:
//
//   `a c` → `(s a c)`   alt 0, selected by its own first set
//   `b c` → `(s b c)`   alt 1
//   `c`   → `(s c)`     alt 0 matches empty, and `C` follows
grammar TransparentNullableFollow;

s : ( ( A? | B ) ) C EOF ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

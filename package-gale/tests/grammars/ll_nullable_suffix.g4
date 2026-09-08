// A tail-greedy Optional whose caller's next sibling is optional on the same
// token: `a`'s `Y?` and alt 0's `b?` both want the `Y`. Both readings complete
// — `(a X Y)` with `b?` empty, or `(a X)` then `(b Y)` — so the decision is an
// ambiguity, and ANTLR4's greedy subrule gives the token to the innermost.
// `ll_greedy_optional_cross_rule.g4` is the same rule with an `else`.
//
//   Input "X Y" → (r (a X Y))   [pinned against the jar]

grammar LlNullableSuffix;

r : (a b? | a) EOF ;
a : X Y? ;
b : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;

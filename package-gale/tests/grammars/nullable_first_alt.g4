// Source: hand-written regression grammar for Gale's rule-alternative order.
// License: same as the Gale package.
//
// `r : A? | B ;` — the first alternative can match nothing, so it is viable
// everywhere and its arm carries no lookahead test. Which alternative ANTLR4
// takes on `b` depends on the caller's continuation, not on the rule:
//
//   'p' r B EOF  on `p b` → `(s p r b)`     alt 0 matches empty, `s` takes B
//   'q' r EOF    on `q b` → `(s q (r b))`   alt 0 followed by EOF cannot, so B
//
// One static decision cannot be both. Gale keeps the unconditional arm in
// alternative order, which is the first answer; the second is the
// full-context decision the ATN simulator makes.
grammar NullableFirstAlt;

s : 'p' r B EOF
  | 'q' r EOF
  ;

r : A? | B ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

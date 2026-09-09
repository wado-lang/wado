// A regression for an optional over an open-ended rule.
//
// `x : . ;` admits every token, so `x? B` has no first set to hold the
// optional back with — what holds it back is what follows it. Taking the
// follow set out of "everything" is a negative test, not a smaller set.
//
//   t : x? B ;   on `b`   → (s (t b))
//                on `a b` → (s (t (x a) b))
grammar OptOpenEndedRule;

s : t EOF ;

t : x? B ;

x : . ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

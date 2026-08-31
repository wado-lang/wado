// Source: hand-written regression grammar for Gale's open-ended nullable alt.
// License: same as the Gale package.
//
// `x : .? ;` is open-ended AND nullable: it admits every token, and it also
// admits none. Measured against the published jar, alt 0 wins every input —
// `x` takes the token where one is there to take, and matches empty where
// taking it would leave `C` nothing:
//
//   ( x | B ) C EOF  on `c`   → (s x c)
//                    on `a c` → (s (x a) c)
//                    on `b c` → (s (x b) c)
grammar OpenEndedNullable;

s : ( x | B ) C EOF ;

x : .? ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

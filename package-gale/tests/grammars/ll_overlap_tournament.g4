// Both alts of `r` start with `A`, so lowering resolves the overlap with a
// scan-side longest-match tournament and raises one OverlapTournament
// diagnostic. Used to exercise the Kiln generator logging it at build time.
grammar OverlapTournament;

r : A B | A C ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

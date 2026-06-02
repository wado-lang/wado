// Source: hand-written regression grammar for Gale prediction diagnostics.
// License: same as the Gale package.
//
// Both alts of `r` start with `A`, so lowering resolves the overlap with a
// scan-side longest-match tournament and raises one OverlapTournament
// diagnostic. Used to exercise the Kiln generator surfacing that warning
// through `KilnHost::emit_diagnostic` at build time.
grammar OverlapTournament;

r : A B | A C ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : [ \t\r\n]+ -> skip ;

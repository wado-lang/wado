// Source: hand-written Gale regression for in-group error recovery.
// License: same terms as the Gale package (see package-gale/README.md).
//
// A multi-element group `(A B C)` inside an alt: a missing middle element
// must be inserted in place (not just deleted/unwound), exercising the
// sync threading in `gen_elements_with_non_greedy`.
grammar GroupRecovery;

s : LP (A B C) RP EOF ;

LP : '(' ;
RP : ')' ;
A  : 'a' ;
B  : 'b' ;
C  : 'c' ;
WS : [ \t\r\n]+ -> skip ;

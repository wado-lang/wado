// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// Both sides of the caller-FOLLOW yield gate in one grammar. `a`'s tail-greedy
// `Y?` competes with the caller's continuation in each alternative; the mask
// the caller threads decides who takes the `Y`.
grammar LlFollowMaskCoverage;

prog : stmt EOF ;

stmt : 'keep' a t ';'
     | 'free' a (Y ';')? ';'
     ;

a : X Y? ;

t : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \t\r\n]+ -> skip ;

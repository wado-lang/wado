// `a`'s tail `Y?` is greedy, but `r`'s continuation after `a` is `Y`. The
// caller-FOLLOW gate must make `a` yield the `Y` to `r` rather than swallow it.
grammar FollowGate;

r : a Y ;
a : X Y? ;

X  : 'X' ;
Y  : 'Y' ;
WS : [ \t\r\n]+ -> skip ;

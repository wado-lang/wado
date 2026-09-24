// ANTLR4's error recovery, one rule per edit it makes: a decision's sync
// (single-token deletion on entry, a skipped run after a loop iteration) and a
// failed rule resynchronising on what the rules under way can continue with.
// `tail` and `single` pin where no sync runs.

grammar AntlrRecovery;

loop   : 'x' 'y'* '!' ;
tail   : tailed '!' ;
tailed : 'x' 'y'* ;
opt    : 'x' 'y'? '!' ;
grp    : 'x' ('y' | 'w') '!' ;
plus   : 'x' 'y'+ '!' ;
twice  : one 'y' one 'w' ;
one    : 'x' | 'q' ;
single : inner 'x' ;
inner  : 'y' ;
multi  : my | mw ;
my     : 'y' ;
mw     : 'w' ;
expr   : e ;
e      : e '+' e | INT | '(' e ')' ;

INT : [0-9]+ ;
Z   : 'z' ;
WS  : ' ' -> skip ;

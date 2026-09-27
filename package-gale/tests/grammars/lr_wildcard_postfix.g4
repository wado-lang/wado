// A `.`-led LR suffix is a catch-all postfix: `e .` absorbs any single trailing
// token. `.` sits at its declared (lowest-here) precedence, so a real operator
// like `'+'` (higher precedence) wins the overlap and only an otherwise-unmatched
// token is absorbed. No operator token names the loop entry, so the runtime ATN
// simulator decides it.
grammar LrWildcardPostfix;
prog : e EOF ;
e : e '+' e | e . | INT ;
INT : [0-9]+ ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

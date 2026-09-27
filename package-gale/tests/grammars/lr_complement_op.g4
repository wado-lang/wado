// A `~X`-led LR suffix is a binary operator over an OPEN operator set: `e ~';'
// e` treats any token except `;` as the infix operator. No operator token names
// the loop entry, so the runtime ATN simulator decides it over the complement
// set; `'*'` is a higher-precedence operator that wins the overlap.
grammar LrComplementOp;
prog : e EOF ;
e : e '*' e | e ~';' e | INT ;
INT : [0-9]+ ;
OP : [+\-/] ;
WS : [ \t\r\n]+ -> skip ;

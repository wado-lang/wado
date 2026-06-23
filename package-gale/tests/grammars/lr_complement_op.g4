// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// A `~X`-led LR suffix is a binary operator over an OPEN operator set: `e ~';'
// e` treats any token except `;` as the infix operator. ANTLR4 rejects this
// (no operator token to climb on), but the semantics is uniquely determined by
// precedence climbing — the loop-entry simply admits any non-`;` token — so
// Gale accepts it as a canonical superset extension. The runtime ATN simulator
// (`atn_first_admits` over the complement set) decides the loop entry; `'*'` is
// a higher-precedence operator that wins the overlap.
grammar LrComplementOp;
prog : e EOF ;
e : e '*' e | e ~';' e | INT ;
INT : [0-9]+ ;
WS : [ \t\r\n]+ -> skip ;

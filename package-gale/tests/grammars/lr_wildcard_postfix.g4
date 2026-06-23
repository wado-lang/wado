// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// A `.`-led LR suffix is a catch-all postfix: `e .` absorbs any single trailing
// token. ANTLR4 rejects it, but the semantics is uniquely determined by
// precedence climbing — `.` admits any token and sits at its declared
// (lowest-here) precedence, so a real operator like `'+'` (higher precedence)
// wins the overlap and only an otherwise-unmatched token is absorbed. Gale
// accepts it as a canonical superset extension; the runtime ATN simulator
// decides the loop entry.
grammar LrWildcardPostfix;
prog : e EOF ;
e : e '+' e | e . | INT ;
INT : [0-9]+ ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

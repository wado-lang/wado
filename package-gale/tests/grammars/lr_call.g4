// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// Postfix call alt (`e '(' (e (',' e)*)? ')'`) nests self-refs inside a
// Group/Repeat in an LR suffix. Those args are PRIMARY references (prec 0):
// `a(b(c))` keeps the inner call and `a(b+c)` admits a lower-precedence arg,
// because each arg's follow (`,` / `)`) holds no LR operator. Regression for
// the `stamp_lr_self_ref_min_prec` nested branch (which used to stamp
// `conflict_min` and lose them). Static dispatch; trees match the published
// ANTLR4 jar.
grammar LrCall;
prog : e EOF ;
e : ID | e '+' e | e '(' (e (',' e)*)? ')' ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

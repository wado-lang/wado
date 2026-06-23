// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// Top-level parenthesised self-ref in an LR suffix (`e '(' e ')'`). The
// bracketed `e` is a PRIMARY reference (prec 0, a full expression) because the
// `)` that follows is not an LR operator — `a(b(c))` keeps the inner group and
// the arg admits a higher-precedence `*`. Regression for the
// `compute_self_ref_prec_from_ops` last-element gate (the bracketed `e` is the
// last self-ref but NOT the alt's last element). Static dispatch; trees match
// the published ANTLR4 jar.
grammar LrParen;
prog : e EOF ;
e : e '*' e | e '(' e ')' | ID ;
ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

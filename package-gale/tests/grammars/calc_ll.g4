// Source: hand-written for Gale's resilient-parser (homogeneous-tree) tests.
// License: same as the Gale package.
//
// An LL(1), non-left-recursive arithmetic grammar: every decision is a
// `Direct` (token-led) dispatch, so it is within Stage 2 scope of the
// homogeneous-tree emitter.
grammar CalcLL;

prog   : expr EOF ;
expr   : term (('+' | '-') term)* ;
term   : factor (('*' | '/') factor)* ;
factor : INT
       | '(' expr ')'
       ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

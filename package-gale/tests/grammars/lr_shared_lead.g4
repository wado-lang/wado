// Source: `lr_atn_trailing.g4` without its atom.
// License: BSD-3-Clause (ANTLR4) — derived from the same descriptor.
//
// `expr 'x' expr` is a prefix of the looser `expr 'x' expr 'y' expr`, so the
// LR loop cannot pick between them on the `'x'` alone.
grammar LrSharedLead;

stat : expr ';' ;

expr : ID
     | expr 'x' expr
     | expr 'x' expr 'y' expr
     ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

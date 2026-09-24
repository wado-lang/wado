// Source: `lr_atn_mid_operand.g4` with the shared token leading the tighter alt.
// License: BSD-3-Clause (ANTLR4) — derived from the same descriptor.
//
// The `'y' expr 'x' expr` atom makes the rule ATN-class. The looser
// `expr 'x' expr 'y' expr` shares `'x'` with the tighter `expr 'x' expr`, and
// its trailing operand is `expr[2]`, so it takes an `x` that follows it.
grammar LrAtnTrailing;

stat : expr ';' ;

expr : ID
     | expr 'x' expr
     | expr 'x' expr 'y' expr
     | 'y' expr 'x' expr
     ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

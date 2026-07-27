// Source: `lr_between.g4` and `lr_mid_operand.g4` combined.
// License: BSD-3-Clause (ANTLR4) — derived from the same descriptor.
//
// Both shapes at once: the `'between' expr 'and' expr` ATOM makes the rule
// ATN-class at its loop entry, and `expr 'between' expr 'and' expr` is the
// mid-operand shape whose operand wants to climb the shared `'and'`. The
// mid-operand continuation gate rides the static LR dispatch, so it does not
// stamp this rule and the simulator decides the loop entry alone — which is
// why the climbing cases still diverge from ANTLR4.
grammar LrAtnMidOperand;

stat : expr ';' ;

expr : ID
     | expr 'and' expr
     | expr 'between' expr 'and' expr
     | 'between' expr 'and' expr
     ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;

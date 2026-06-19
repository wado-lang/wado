// Source: hand-written for Gale's resilient-parser left-recursion tests.
// License: same as the Gale package.
//
// Direct left recursion with precedence: `*` binds tighter than `+`. ANTLR4 /
// Gale rewrite this into a precedence-climbing parser.
grammar ArithLR;

expr : expr '*' expr   # Mul
     | expr '+' expr   # Add
     | INT             # Num
     ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

grammar Expr;

program: expr EOF;

expr
   : ID
   | 'not' expr
   | expr 'and' expr
   | expr 'or' expr
   ;

ID: [a-zA-Z_][a-zA-Z_0-9]*;
WS: [ \t\n\r\f]+ -> skip;
ERROR: .;

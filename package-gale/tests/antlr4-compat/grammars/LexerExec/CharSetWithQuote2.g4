lexer grammar L;
A : ["\\ab]+ {System.out.println("A");} ;
WS : [ \n\t]+ -> skip ;

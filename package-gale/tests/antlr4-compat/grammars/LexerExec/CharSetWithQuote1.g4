lexer grammar L;
A : ["a-z]+ {System.out.println("A");} ;
WS : [ \n\t]+ -> skip ;

lexer grammar L;
I : [0-9]+ {System.out.println("I");} ;
ID : [a-zA-Z] [a-zA-Z0-9]* {System.out.println("ID");} ;
WS : [ \n\u0009\r]+ -> skip ;

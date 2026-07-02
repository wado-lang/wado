lexer grammar L;
I : '0'..'9'+ {System.out.println("I");} ;
WS : [ \n\u000D]+ -> skip ;

lexer grammar L;
I : (~[ab \n]|'a')  {System.out.println("I");} ;
WS : [ \n\u000D]+ -> skip ;

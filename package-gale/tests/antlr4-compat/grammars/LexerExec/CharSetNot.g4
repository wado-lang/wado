lexer grammar L;
I : ~[ab \n] ~[ \ncd]* {System.out.println("I");} ;
WS : [ \n\u000D]+ -> skip ;

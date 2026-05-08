lexer grammar L;
I : ~[ab \n] ~[ \ncd]* {<writeln("\"I\"")>} ;
WS : [ \n\u000D]+ -> skip ;

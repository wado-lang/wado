lexer grammar L;
I : (~[ab \\n]|'a')  {<writeln("\"I\"")>} ;
WS : [ \n\\u000D]+ -> skip ;

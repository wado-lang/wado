lexer grammar L;
A : ["a-z]+ {<writeln("\"A\"")>} ;
WS : [ \n\t]+ -> skip ;

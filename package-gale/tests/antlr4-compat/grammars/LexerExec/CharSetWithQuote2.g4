lexer grammar L;
A : ["\\ab]+ {<writeln("\"A\"")>} ;
WS : [ \n\t]+ -> skip ;

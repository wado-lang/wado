lexer grammar L;
DASHBRACK : [\\-\]]+ {<writeln("\"DASHBRACK\"")>} ;
WS : [ \n]+ -> skip ;

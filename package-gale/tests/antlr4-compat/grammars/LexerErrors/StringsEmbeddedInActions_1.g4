lexer grammar L;
ACTION2 : '[' (STRING | ~'"')*? ']';
STRING : '"' ('\\' '"' | .)*? '"';
WS : [ \t\r\n]+ -> skip;

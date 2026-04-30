lexer grammar L;
ACTION : '{' (ACTION | ~[{}])* '}';
WS : [ \r\n\t]+ -> skip;

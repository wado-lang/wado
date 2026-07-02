lexer grammar L;
ENUM : [a-z]+  { false }? ;
ID : [a-z]+  ;
WS : (' '|'\n') -> skip;

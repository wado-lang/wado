lexer grammar L;
ENUM : [a-z]+  { getText().equals("enum") }? ;
ID : [a-z]+  ;
WS : (' '|'\n') -> skip;

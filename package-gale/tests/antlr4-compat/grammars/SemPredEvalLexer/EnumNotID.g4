lexer grammar L;
ENUM : [a-z]+  { <TextEquals("enum")> }? ;
ID : [a-z]+  ;
WS : (' '|'\n') -> skip;

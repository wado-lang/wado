lexer grammar L;
ENUM : [a-z]+  { <False()> }? ;
ID : [a-z]+  ;
WS : (' '|'\n') -> skip;

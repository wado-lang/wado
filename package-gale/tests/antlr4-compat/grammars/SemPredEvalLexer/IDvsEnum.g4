lexer grammar L;
ENUM : 'enum' { <False()> }? ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip;

lexer grammar L;
ENUM : 'enum' { false }? ;
ID : 'a'..'z'+ ;
WS : (' '|'\n') -> skip;

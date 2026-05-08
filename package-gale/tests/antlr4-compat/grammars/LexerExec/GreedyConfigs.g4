lexer grammar L;
I : ('a' | 'ab') {<Text():writeln()>} ;
WS : (' '|'\n') -> skip ;
J : .;

lexer grammar L;
I : ('a' | 'ab') {System.out.println(getText());} ;
WS : (' '|'\n') -> skip ;
J : .;

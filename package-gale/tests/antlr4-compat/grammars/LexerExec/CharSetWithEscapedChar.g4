lexer grammar L;
DASHBRACK : [\-\]]+ {System.out.println("DASHBRACK");} ;
WS : [ \n]+ -> skip ;

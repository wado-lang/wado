lexer grammar L;
WORD1 : ID1+ { System.out.println(getText()); } ;
WORD2 : ID2+ { System.out.println(getText()); } ;
fragment ID1 : { getCharPositionInLine() < 2 }? [a-zA-Z];
fragment ID2 : { getCharPositionInLine() >= 2 }? [a-zA-Z];
WS : (' '|'\n') -> skip;

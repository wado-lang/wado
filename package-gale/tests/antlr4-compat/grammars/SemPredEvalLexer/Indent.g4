lexer grammar L;
ID : [a-z]+  ;
INDENT : [ \t]+ { _tokenStartCharPositionInLine == 0 }?
{ System.out.println("INDENT"); }  ;
NL : '\n';
WS : [ \t]+ ;

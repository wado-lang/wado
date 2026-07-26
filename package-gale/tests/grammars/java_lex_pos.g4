// Source: Gale test fixture (Stage C: lexer char-position attributes)
// License: same as the Gale package
//
// The two position accessors ANTLR4 exposes to a lexer predicate:
// `_tokenStartCharPositionInLine` is the column the token began at, while
// `getCharPositionInLine()` is the live match cursor's column.
lexer grammar JavaLexPos;

INDENT : [ ]+ { _tokenStartCharPositionInLine == 0 }? {System.out.println("INDENT");} ;
LATE : { getCharPositionInLine() >= 2 }? [a-z]+ ;
ID : [a-z]+ ;
WS : ' ' ;
NL : '\n' ;

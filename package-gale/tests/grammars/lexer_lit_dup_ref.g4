// Source: Gale test fixture (a lexer rule referenced by another)
// License: same as the Gale package
//
// The parser literal `'kw'` is an alias for `K`, and `Z` references `K`, so
// `K` must keep a matcher both paths can reach.
grammar LexerLitDupRef;

s : 'kw' | Z ;

K  : 'kw' ;
Z  : 'z'+ K ;
WS : ' ' -> skip ;

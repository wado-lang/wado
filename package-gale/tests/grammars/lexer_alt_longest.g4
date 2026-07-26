// Source: Gale test fixture (lexer alternation maximal munch)
// License: same as the Gale package
//
// ANTLR4's lexer simulates the whole rule, so an alternation in tail position
// yields the longest alternative. One with a suffix after it keeps first-match:
// `K` must still match `xyz` by taking the short arm.
lexer grammar LexerAltLongest;

I : ('a' | 'ab') ;
K : ('x' | 'xy') 'yz' ;
M : 'm' ('a' | 'ab') ;
N : 'n' | 'no' | 'nop' ;
J : . ;

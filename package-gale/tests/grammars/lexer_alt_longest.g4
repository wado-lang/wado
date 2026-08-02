// Source: Gale test fixture (lexer alternation maximal munch)
// License: same as the Gale package
//
// ANTLR4's lexer simulates the whole rule, so an alternation in tail position
// yields the longest alternative. One with a suffix after it is decided by the
// suffix instead: `K` must match `xyz` by taking the SHORT arm, since `xy`
// would strand the `yz`. See lexer_alt_suffix_longest.g4 for that half.
lexer grammar LexerAltLongest;

I : ('a' | 'ab') ;
K : ('x' | 'xy') 'yz' ;
M : 'm' ('a' | 'ab') ;
N : 'n' | 'no' | 'nop' ;
J : . ;

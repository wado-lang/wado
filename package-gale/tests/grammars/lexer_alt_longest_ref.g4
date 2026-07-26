// Source: Gale test fixture (lexer rule reference into a maximal-munch rule)
// License: same as the Gale package
//
// `X`'s own alternation is in tail position *of X*, but `Y` calls `X` from a
// non-tail position: taking X's longest arm there strands `Y`'s suffix. A rule
// another lexer rule calls must therefore keep the first-match emit.
lexer grammar LexerAltLongestRef;

X : 'a' | 'ab' ;
Y : X 'bc' ;
J : . ;

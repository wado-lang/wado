// Source: Gale test fixture (lexer alternation maximal munch)
// License: same as the Gale package
//
// ANTLR4's lexer matches a rule via NFA→DFA simulation, so an alternation in
// tail position yields the LONGEST matching alternative, not the first one
// (`I : ('a' | 'ab')` matches `ab`). An alternation with a suffix after it
// keeps first-match: `K` must still match `xyz` by taking the short arm.
lexer grammar LexerAltLongest;

I : ('a' | 'ab') ;
K : ('x' | 'xy') 'yz' ;
M : 'm' ('a' | 'ab') ;
N : 'n' | 'no' | 'nop' ;
J : . ;

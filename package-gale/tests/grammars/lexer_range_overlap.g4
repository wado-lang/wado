// Source: Gale test fixture (overlapping lexer first-char ranges)
// License: same as the Gale package
//
// `A` and `B` open on overlapping-but-unequal char ranges. A char in the
// intersection (`d`..`f`) must try both rules — dispatching on the first
// range group alone would shadow `B` for exactly those chars, and the rules
// only diverge after their first char, so the shadowing rejects valid input.
lexer grammar LexerRangeOverlap;

A : [a-f] '1' ;
B : [d-z] '2' ;
WS : ' ' -> skip ;

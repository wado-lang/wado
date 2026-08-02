// Source: Gale test fixture (lexer alternation with a suffix)
// License: same as the Gale package
//
// ANTLR4's lexer simulates the whole rule, so an alternation followed by a
// suffix must take the arm that lets the WHOLE rule match, not the first arm
// that matches on its own.
//
// `I` on `abbc`: the `'a'` arm matches first, but then `'bc'` faces `bb` and
// the rule fails. Only the `'ab'` arm leaves a `bc` behind. (`abc` matches
// through the `'a'` arm, so it is not a repro.)
//
// `K` is the mirror case — there the SHORT arm is the one that works, so an
// emit that simply preferred the longest arm would break it.
//
// `P` makes the choice depend on the suffix rather than on arm length: the
// short arm reaches further overall (`'a'` + `'bcd'`) than the long one
// (`'ab'` + `'c'`), which is what maximal munch means when the suffix is
// itself an alternation.
lexer grammar LexerAltSuffixLongest;

I : ('a' | 'ab') 'bc' ;
K : ('x' | 'xy') 'yz' ;
P : ('p' | 'pq') ('qrs' | 'r') ;
J : . ;

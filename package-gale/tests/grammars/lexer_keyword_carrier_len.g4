// Source: Gale test fixture (keyword shortcut carrier reachability)
// License: same as the Gale package
//
// The keyword classifier is an optimisation: a keyword-shaped rule is left out
// of the dispatch and instead rewrites a CARRIER rule's match, once a carrier
// has matched the same characters. Admission checks that some later carrier
// covers the keyword's first character — but covering the first character does
// not mean the carrier can match the whole keyword.
//
// `D` — neither `A` nor `WORD` starts at `h`, so its only carrier is `C : .`.
// `C` matches one char, so the classifier is only ever asked about `h`, never
// `h2`.
//
// `E` — the same hole reached through two alternatives of one carrier.
// `MULTI` covers `g` through `[g-h]` and reaches three chars through `'qrs'`,
// but never both at once, so it can no more produce `gn` than `C` can. A bound
// taken as the LONGEST alternative would admit it.
//
// `IF` is the sound half — `WORD` covers `i` AND can match the whole `if`, so
// the shortcut holds and `IF` must stay out of the dispatch.
lexer grammar LexerKeywordCarrierLen;

A : [a-f] '1' ;
D : 'h' '2' ;
IF : 'i' 'f' ;
E : 'g' 'n' ;
WORD : [i-z] [a-z]* ;
MULTI : [g-h] | 'qrs' ;
C : . ;

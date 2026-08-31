// Source: hand-written regression grammar for Gale's open-ended alternative.
// License: same as the Gale package.
//
// `r` is open-ended, but through a rule reference: its first set is empty for
// the reason a surface `.`'s is, not because it matches nothing. Answering
// that from the surface elements alone makes the parse and the scan partition
// the group differently.
//
//   `a b` → `(s (r a b))`  the open-ended alternative takes both tokens
//   `a`   → `(s a)`        it cannot complete, so `A` does
grammar IndirectWildcard;

s : ( r | A ) EOF ;

r : . B ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

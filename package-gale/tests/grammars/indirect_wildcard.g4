// Source: hand-written regression grammar for Gale's open-ended alternative.
// License: same as the Gale package.
//
// `r` is open-ended, but through a rule reference: its first set is empty for
// the reason a surface `.`'s is, not because it matches nothing. Answering
// that from the surface elements said yes for `.` written in place and no
// here, so the parse side and the scan side partitioned the group differently
// — the parse committed to `r` and the scan measured the `A` alternative.
//
//   `a b` → `(s (r a b))`  the open-ended alternative takes both tokens
//   `a`   → `(s a)`        it cannot complete, so `A` does
grammar IndirectWildcard;

s : ( r | A ) EOF ;

r : . B ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

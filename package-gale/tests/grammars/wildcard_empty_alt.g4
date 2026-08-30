// Source: hand-written regression grammar for Gale's overlap partition.
// License: same as the Gale package.
//
// A wildcard admits every token, so no lookahead test separates any
// alternative from it: all three share one branch and the scan lengths decide,
// with the empty alternative ranked as epsilon. Left out of that merge it
// became a branch of its own, behind the wildcard's unconditional one, and no
// input could reach it. Measured against the published jar:
//
//   ``      → `(s r)`       nothing scans, so the empty alternative wins
//   `a`     → `(s (r a))`   `A` and `.` tie at one token; lowest index wins
//   `b`     → `(s (r b))`   only `.` scans
//   `a b`   → `(s (r a) b)` the `B?` after `r` still gets its token
grammar WildcardEmptyAlt;

s : r B? EOF ;

r : ( A | . | ) ;

A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

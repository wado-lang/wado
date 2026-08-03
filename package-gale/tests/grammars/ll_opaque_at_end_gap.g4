// Soundness gap: an at-end alternative dropped by the opaque-rule
// expansion path.
//
// `e`'s three alts all open on `X`. Alts 0 and 1 enter multi-token rules
// (`a`, `b`), so the SLL walk marks them opaque and hands the token to
// `try_expand_opaque`, which expands `a` / `b` one token deep and finds
// `Y` and `Z` separate them. Alt 2 is complete after `X`: its FIRST set
// at that depth is empty, so the token loop never sees it, and the
// coverage check only verifies the opaque alts:
//
//   Dispatch[d=0] [TK_X] -> Dispatch[d=1] [TK_Y] -> alt 0
//                                         [TK_Z] -> alt 1
//
// Nothing claims a bare `X`, and the fallback tournament is seeded from
// the branches, so alt 2 is unreachable and a valid input is rejected.

grammar LlOpaqueAtEndGap;

r : e EOF ;
e : a
  | b
  | X
  ;
a : X Y ;
b : X Z ;

X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
WS : [ \r\n\t]+ -> skip ;

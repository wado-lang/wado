// An alternative that ends where the others continue, behind a shared opaque
// prefix.
//
// `e`'s three alts all open on `X`. Alts 0 and 1 enter multi-token rules, so
// the SLL walk expands them one token deep and finds `Y` / `Z` separate them.
// Alt 2 is complete after `X`, so it claims no token of its own at that depth
// and a dispatch on `Y` / `Z` would leave it unreachable:
//
//   Dispatch[d=0] [TK_X] -> Dispatch[d=1] [TK_Y] -> alt 0
//                                         [TK_Z] -> alt 1
//
// Nothing there claims a bare `X`, and the fallback tournament is seeded from
// the branches. `r : e EOF` is the half where no caller continues on `Y` / `Z`,
// so the decision belongs to the tournament; `ll_opaque_at_end_context.g4` is
// the half that needs the simulator.

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

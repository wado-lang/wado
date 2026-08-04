// The context-dependent half of `ll_opaque_at_end_gap.g4`: there the tokens
// the expanded opaque alts branch on cannot follow `e` at any call site, so
// the at-end alt is safely left to the longest-match tournament. Here `Y`
// both continues alt 0 (`a : X Y`) and follows `e` in its caller, so which
// alt is right depends on the caller's continuation, not on lookahead:
//
//   `X Y Y` — `e` must take `a`, leaving the second `Y` for `r`
//   `X Y`   — `e` must take the bare `X`, leaving the `Y` for `r`
//
// A longest-match tournament always picks `a`, so the second input needs the
// simulator (ATN-class site 3 in antlr4-compatibility.md). The prediction
// walk must therefore report this as an at-end conflict, not as a resolved
// dispatch on `Y` / `Z`.

grammar LlOpaqueAtEndContext;

r : e Y EOF ;
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

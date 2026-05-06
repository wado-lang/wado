// LL(*) gap: tail-greedy needs follow info from a CALLER OF THE CALLER.
//
// Pattern: `r` invokes `wrapper`, which in turn invokes `a`. `wrapper`
// is just a passthrough — its body is exactly `a`, with no required
// siblings of its own. So when `a` runs, the immediate sibling list at
// its call site (inside `wrapper`) is empty.
//
// In `r`'s alt 0 (`wrapper b`), `wrapper`'s caller-side follow is
// `FIRST(b) = {Y}`. To suppress `a`'s greedy `Y?` we need to propagate
// that follow from `r`'s alt down through `wrapper`'s call site into
// `a`'s tail-greedy analysis.
//
//   Input "X Y":
//     ANTLR4 LL → (r (wrapper (a X)) (b Y))
//     Gale v1   → no variant registered (ruleref_call_follow at the
//                 inner `a` site is `[]`) → greedy `a` swallows Y
//
// Gap: compute a per-rule FOLLOW set as the fixed point of all call
// sites' caller-local follows, plus deep propagation through nullable
// suffixes. Then `intern_follow_variant` for `a` should see {Y, …}
// transitively.

grammar LLCtxFollow;

r : (wrapper b | wrapper) EOF ;

wrapper : a ;
a       : X Y? ;
b       : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;

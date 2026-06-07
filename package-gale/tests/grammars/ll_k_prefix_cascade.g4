// LL(*) gap: the multi-token tail-greedy `Repeat` lives one RuleRef
// hop away from the call site, behind a MULTI-ALT rule.
//
// `a` ends in `RuleRef m`; `m` is a multi-alt rule whose two alts share
// a leading `K` (so `m`'s dispatch routes through the `_bt_<n>` scan
// tournament, not the simple peek-dispatch), and whose first alt
// carries the multi-token tail-greedy `(X Y)+`.
//
// At the `a` call site in `r`, the caller's deterministic K-prefix is
// `[{X}, {Y}, {W}]` (deep-walked through the single-alt `c : X Y W`).
// For the variant to fire, `tail_greedy_k_prefix_of_rule("a")` must see
// through the `m` RuleRef to the `(X Y)+` loop. The K-prefix cascade
// previously halted at the `RuleRef` arm of
// `tail_greedy_k_prefix_of_element` out of conservatism (the multi-alt
// variant dispatcher used to call the non-variant `parse_m_bt_<n>`
// helpers). Now that the dispatcher routes to the variant's own
// `parse_m__follow_<id>_bt_<n>` / `scan_m__follow_<id>_<n>` helpers
// (covered by `ll_multi_alt_overlap.g4`), the gate can flow through
// `m` cleanly: `a` registers a (transparent) K-prefix variant that
// threads the mask down to `m`, whose `(X Y)+` loop then gates on the
// caller's prefix.
//
//   Input "N K X Y X Y W":
//     ANTLR4 LL → (r (a N (m K X Y)) (c X Y W))   — `m`'s loop stops
//                                                    after one iter
//     Gale pre-fix → `(X Y)+` runs greedy, eats both `X Y` blocks, so
//                    `c` is starved of its `X Y W` → parse error.

grammar LLKPrefixCascade;

r : a c EOF ;
a : N m ;
m : K (X Y)+
  | K Z
  ;
c : X Y W ;

N : 'N' ;
K : 'K' ;
X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
W : 'W' ;
WS : [ \r\n\t]+ -> skip ;

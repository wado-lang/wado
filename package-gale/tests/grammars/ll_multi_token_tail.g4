// LL(*) gap: callee whose TAIL-GREEDY `Repeat` has a MULTI-TOKEN inner.
//
// Source: shape derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/PredictionMode_LL.txt (the `(a b | a)` decision idea),
//   widened so the callee's tail-greedy is `(X Y)+` rather than `Y?`.
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.
//
// Pattern: callee `a` ends in `(X Y)+`, whose inner consumes TWO tokens
// per iteration. Caller `r` calls `a` then a sibling `c` whose FIRST is
// also `X` (here `c : X Y Z`). ANTLR4 LL knows the final iteration
// belongs to `c`, not `a`. Greedy SLL keeps the loop running and
// starves `c`.
//
// Today Gale rejects multi-token-inner `Repeat`s in
// `tail_greedy_first_of_element` (soundness invariant 1 in
// `antlr4-compatibility.md`), so no `__follow_<id>` variant for `a` is
// registered and the loop runs greedy.
//
//   Input "N X Y X Y Z":
//     ANTLR4 LL → (r (a N X Y) (c X Y Z))   — `a` stops after 1 iter
//     Gale v1   → parse error                — `a` eats 2 iters → `c` starved

grammar LLMultiTokenTail;

r : a c EOF ;
a : N (X Y)+ ;
c : X Y Z ;

N : 'N' ;
X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
WS : [ \r\n\t]+ -> skip ;

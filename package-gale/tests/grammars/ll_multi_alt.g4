// LL(*) gap: tail-greedy callee defined as a MULTI-ALT rule.
//
// Pattern: like `ll_basic.g4` but `a` itself has multiple alternatives.
// `intern_follow_variant` rejects multi-alt rules in v1 because
// `gen_parse_follow_variant` only knows how to emit single-alt
// `gen_alt_body` shapes. So even though tail_greedy(a) ⊃ {Y} and the
// caller's follow at alt 0 is {Y}, no `parse_a__follow_<id>` is
// generated and the call site falls back to greedy `parse_a(p)`.
//
//   Input "X Y":
//     ANTLR4 LL → (r (a X) (b Y))
//     Gale v1   → variant skipped → greedy a swallows Y → wrong tree
//
// Gap: extend `gen_parse_follow_variant` to emit `parse_<rule>_bt_<n>`-
// shaped multi-alt bodies. The scan-side multi-alt emit already works
// (variant emission is shared with `gen_scan_function_named`), so only
// the parse-side needs catching up.

grammar LLMultiAlt;

r : (a b | a) EOF ;
a : X Y?
  | Z
  ;
b : Y ;

X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
WS : [ \r\n\t]+ -> skip ;

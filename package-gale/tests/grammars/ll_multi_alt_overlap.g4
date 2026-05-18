// LL(*) gap: tail-greedy callee is a MULTI-ALT rule whose alts share
// their FIRST set so the rule body goes through the prediction
// (`_bt_<N>` tournament) codepath rather than the simple peek-dispatch
// in `gen_multi_alt_body`.
//
// `a` has two alts both starting with `X`, so prediction emits
// `parse_a_bt_0` / `parse_a_bt_1` helpers and a per-token dispatcher
// that picks among them. When `a` is reached through a tail-greedy
// call site (caller's FOLLOW contains the trailing `Y`), the follow-
// aware variant emits `parse_a__follow_<id>_bt_<N>` per-alt helpers
// — but TODO #3 ("Multi-alt variant dispatcher emit") notes the
// variant dispatcher still calls the NON-variant `parse_a_bt_<N>`
// helpers, so the variant body's Y-suppression never runs.
//
//   Input "X X Y":
//     ANTLR4 LL → (r (a X (a X)) (b Y))
//     Gale pre-fix → variant dispatcher delegates to greedy a → a
//                    swallows the trailing Y → wrong tree (and the
//                    trailing `b Y` never matches).
//
// The grammar deliberately keeps `a`'s alts overlapping so the
// `has_overlaps` branch in `gen_parse_fn_named` fires; that's what
// routes emission through `gen_multi_alt_body_bt` →
// `gen_prediction_code`.

grammar LLMultiAltOverlap;

r : (a b | a) EOF ;
a : X a Y?
  | X
  ;
b : Y ;

X : 'X' ;
Y : 'Y' ;
WS : [ \r\n\t]+ -> skip ;

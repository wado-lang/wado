// Source: hand-written regression for ATN-class multi-alt prediction.
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// On `a b c`, `x` must pick alt0 (`'a' 'b'`) so the caller `s` gets its `'c'`
// (AtEndConflict: longest-match would grab the `'c'`).
grammar LlLongestVsContext;

s : x 'c' ;
x : 'a' 'b'
  | 'a' 'b' 'c'
  ;
WS : ' ' -> skip ;

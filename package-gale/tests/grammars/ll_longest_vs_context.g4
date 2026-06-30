// Source: hand-written regression for ATN-class multi-alt prediction.
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// `x`'s two alts share the prefix `'a' 'b'`; alt1 also consumes a trailing
// `'c'`. The longest-match scan tournament always prefers alt1 (3 tokens),
// but on input `a b c` the caller `s` needs that `'c'` — so the correct
// (ANTLR4 ALL(*)) choice is alt0 (`'a' 'b'`), leaving `'c'` for `s`. A pure
// longest-match tournament mis-parses this; the runtime ATN simulator,
// driven by the caller continuation, picks alt0.
grammar LlLongestVsContext;

s : x 'c' ;
x : 'a' 'b'
  | 'a' 'b' 'c'
  ;
WS : ' ' -> skip ;

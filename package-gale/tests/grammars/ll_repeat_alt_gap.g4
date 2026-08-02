// Soundness gap: a `Repeat` competing with a sibling alternative on the
// same leading token.
//
// `a`'s two alts both open on `X`, so lowering picks a `Tournament`
// (longest-match scan) dispatch. Emit builds its own SLL prediction tree
// instead, and that walk advances a `X+` config straight past the repeat
// — the "still in the loop" reading is never generated. The tree then
// looks fully resolved:
//
//   Dispatch[d=0] [TK_X] -> Dispatch[d=1] [TK_Y] -> alt 0
//                                         [TK_Z] -> alt 1
//
// No branch claims a second `TK_X`, and the emitted cascade has no
// else, so `X X Y` reaches `no_viable` and a valid input is rejected.
// The lowered `Tournament` would have parsed it — it is simply never
// reached.

grammar LlRepeatAltGap;

r : a EOF ;
a : X+ Y
  | X Z
  ;

X : 'X' ;
Y : 'Y' ;
Z : 'Z' ;
WS : [ \r\n\t]+ -> skip ;

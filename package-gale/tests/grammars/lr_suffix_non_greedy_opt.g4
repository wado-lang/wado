// Source: hand-written for Gale's left-recursion tests (the LR-suffix twin of
//   lr_dangling_else.g4, whose `??` sits in an atom alternative).
// License: same as the Gale package.
//
// A non-greedy `??` inside a left-recursive SUFFIX alternative. The suffix body
// is emitted by the op-only walker, which reads the lowered `RepeatOp` rather
// than the surface element, so it must learn the prefer-skip decision from the
// op. `s` needs the trailing `else ID` for itself, so the `??` has to skip.
grammar LrSuffixNonGreedyOpt;

s : e 'else' ID EOF ;
e : ID
  | e 'if' e ('else' e)??
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

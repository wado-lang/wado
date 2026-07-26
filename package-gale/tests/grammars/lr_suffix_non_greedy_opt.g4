// Source: hand-written for Gale's left-recursion tests (the LR-suffix twin of
//   lr_dangling_else.g4, whose `??` sits in an atom alternative).
// License: same as the Gale package.
//
// A non-greedy `??` inside a left-recursive SUFFIX alternative, whose body the
// op-only walker emits from the lowered op alone — so prefer-skip has to reach
// it through the op. `s` needs the trailing `else ID` for itself.
grammar LrSuffixNonGreedyOpt;

s : e 'else' ID EOF ;
e : ID
  | e 'if' e ('else' e)??
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

// Source: distilled from SQLite.g4's `expr : … | expr K_NOT? K_BETWEEN expr K_AND expr`.
// License: same as the Gale package.
//
// The LR-suffix twin of `lr_between.g4`: the `between` operand sits INSIDE a
// left-recursive alternative, not in an atom alt. ANTLR4 rewrites a mid-alt
// self-reference as `e[0]` — a full sub-expression — so it climbs the shared
// `and` delimiter as long as the enclosing alternative can still find its own
// `and`. Gale matches it by stamping the operand `e[0]` too and gating each
// loop iteration on the alternative's remaining continuation.
grammar LrMidOperand;

s : e EOF ;
e : ID
  | e 'and' e
  | e 'between' e 'and' e
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

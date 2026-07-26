// Source: distilled from SQLite.g4's `expr : … | expr K_NOT? K_BETWEEN expr K_AND expr`.
// License: same as the Gale package.
//
// The LR-suffix twin of `lr_between.g4`: the `between` operand sits INSIDE a
// left-recursive alternative, not in an atom alt. ANTLR4 rewrites a mid-alt
// self-reference as `e[0]` — a full sub-expression — so it climbs the shared
// `and` delimiter as long as the enclosing alternative can still find its own
// `and`. Gale stamps the alternative's own precedence there instead, which stops
// the middle operand at the first operand and re-brackets the tree: the driver
// test's climbing case is `#[TODO]`, and TODO.md records why the fix (routing the
// rule through the simulator) is priced out for a hot expression rule.
grammar LrMidOperand;

s : e EOF ;
e : ID
  | e 'and' e
  | e 'between' e 'and' e
  ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;

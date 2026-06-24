// Source: hand-written for Gale's left-recursion tests.
// License: same as the Gale package.
//
// Non-greedy `??` inside a left-recursive rule: the dangling-else binds to the
// nearest `if` (`if 1 then if 2 then 3 else 4` → the else is the inner if's).
// The `??` enter/skip is decided by the runtime ATN simulator with the rule's
// live `min_prec` threaded through `atn_ng_optional_enter` (the atom fn now
// carries `min_prec`), so an enter edge that would climb an LR suffix below
// the current precedence is pruned. Regression for the former
// `atn_optional_enter_cond` "no `??` inside an LR rule" assert. Trees match the
// published ANTLR4 jar.
grammar LrDanglingElse;
prog : e EOF ;
e : 'if' e 'then' e ('else' e)??
  | e '*' e
  | INT
  ;
INT : [0-9]+ ;
WS : [ \t\r\n]+ -> skip ;

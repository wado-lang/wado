// Source: hand-written regression grammar for an optional over an epsilon rule.
// License: same as the Gale package.
//
// `x : ;` derives only the empty string, so no token selects `x?` — and it
// matches anyway, here and everywhere, because matching nothing is what it
// does. The jar enters it and produces the empty node.
//
//   'k' x? A EOF   on `k a` → (s k x a)
//
// Reading the absent first set as "can never match" dropped the optional from
// the parse entirely; asserting the case away instead aborted the generator on
// the left-recursive spelling below, which reaches a different emitter.
grammar OptEpsilonRule;

s : 'k' x? A EOF
  | e EOF
  ;

e : e '+' x? A
  | A
  ;

x : ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;

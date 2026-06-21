// Source: hand-written for Gale's resilient-parser non-greedy tests.
// License: same as the Gale package.
//
// Non-greedy wildcard repetition: `.*?` (alt 0) and `.+?` (alt 1) consume any
// tokens until the closing delimiter can fire, instead of greedily swallowing
// it. Statically decidable, so no runtime ATN simulator is needed.
grammar NonGreedy;

start : seg* EOF ;

seg   : '(' .*? ')'
      | '[' .+? ']'
      ;

INT  : [0-9]+ ;
NAME : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;

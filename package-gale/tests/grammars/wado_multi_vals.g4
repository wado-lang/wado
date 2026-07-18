// Source: Gale test fixture (Stage C multi-alt value channel)
// License: same as the Gale package
//
// A multi-alt (non-LR) rule with a `returns` value channel: each alt writes
// its own `$v`, threaded back through the token-led dispatch, and a caller
// reads `$e.v` cross-rule.
grammar WadoMultiVals;

options { language = Wado; }

r : e { p.emit(`v=${$e.v}`); } ;

e returns [i32 v] : A { $v = 1; }
  | B { $v = 2; }
  | C { $v = 3; }
  ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
WS : ' ' -> skip ;

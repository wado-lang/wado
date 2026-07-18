// Source: Gale test fixture (Stage C left-recursive value channel)
// License: same as the Gale package
//
// The corpus LR-binary shape: a left-recursive rule computes a value from its
// two operands. The leading self-ref (`l=e`) is the accumulated left operand,
// the trailing self-ref (`r=e`) the recursively-parsed right operand, and `$v`
// the value of this continuation. Precedence follows alt order (`*` before `+`).
grammar WadoLrVals;

options { language = Wado; }

s : x=e { p.emit(`=${$x.v}`); } ;

e returns [i32 v]
  : l=e '*' r=e { $v = $l.v * $r.v; }
  | l=e '+' r=e { $v = $l.v + $r.v; }
  | n=INT { $v = $n.int; }
  ;

INT : [0-9]+ ;
WS : ' ' -> skip ;

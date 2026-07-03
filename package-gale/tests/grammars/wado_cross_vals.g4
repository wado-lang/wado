// Source: Gale test fixture (Stage C cross-rule value channel)
// License: same as the Gale package
//
// The corpus's LR-binary shape in miniature: a rule reads its labeled child
// rules' returned values. `$a.v` / `$b.v` resolve to the child bindings the
// call sites created (`let a = _parse_e(p)`), so `e` must return its vals.
grammar WadoCrossVals;

options { language = Wado; }

r : a=e '+' b=e { p.emit(`{$a.v + $b.v}`); } ;

e returns [i32 v] : n=INT { $v = 1; } ;

INT : [0-9]+ ;
WS : ' ' -> skip ;

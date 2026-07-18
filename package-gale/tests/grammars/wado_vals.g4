// Source: Gale test fixture (Stage C value channel)
// License: same as the Gale package
//
// A `language = Wado` grammar whose rule declares a `returns` value channel.
// One action writes `$v`, a later action reads it back through a template and
// emits the result, exercising substitution ($v -> vals.v) end to end.
grammar WadoVals;

options { language = Wado; }

r returns [i32 v] : A { $v = 5; } B { p.emit(`v=${$v}`); } ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

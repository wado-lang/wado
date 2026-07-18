// Source: Gale test fixture (Stage C rule arguments)
// License: same as the Gale package
//
// A rule argument (`e[i32 base]`) is threaded as a `_parse_e` parameter and
// stored into the rule's value channel, so `$base` reads the value the caller
// passed at `e[3]`. A second call passes a caller-side expression.
grammar WadoRuleArg;

options { language = Wado; }

r : e[3] e[40] ;

e[i32 base] : A { p.emit(`base=${$base} `); } ;

A : 'a' ;
WS : ' ' -> skip ;

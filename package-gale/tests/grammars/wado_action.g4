// Source: Gale test fixture (Stage C action execution)
// License: same as the Gale package
//
// A `language = Wado` grammar with a print-style action: the action runs
// during the parse and its text lands in `ParseResult.output`.
grammar WadoAction;

options { language = Wado; }

r : A { p.emit("hi"); } B { p.emit("!"); } ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

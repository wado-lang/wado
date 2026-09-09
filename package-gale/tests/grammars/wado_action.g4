// A `language = Wado` grammar with a print-style action: the action runs
// during the parse and its text lands in `ParseResult.output`.
grammar WadoAction;

options { language = Wado; }

r : A { p.emit("hi"); } B { p.emit("!"); } ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

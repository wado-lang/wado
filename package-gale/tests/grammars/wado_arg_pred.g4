// Source: Gale test fixture (Stage C context-dependent predicate on a rule arg)
// License: same as the Gale package
//
// An alt-initial predicate reads a threaded rule argument (`$mode`), so the
// caller's `e[1]` selects the predicated alternative.
grammar WadoArgPred;

options { language = Wado; }

s : e[1] ;

e[i32 mode]
    : {$mode == 1}? A { p.emit("one"); }
    | B { p.emit("b"); }
    ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

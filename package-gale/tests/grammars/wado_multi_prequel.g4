// Source: Gale test fixture (Stage C @init / @after on a multi-alt rule)
// License: same as the Gale package
//
// `@init` runs at rule entry before the dispatch, `@after` after the chosen
// alt's body — both on a multi-alt rule, sharing its value channel. @init seeds
// `$v`, the selected alt updates it, @after reads the final value.
grammar WadoMultiPrequel;

options { language = Wado; }

r returns [i32 v]
    @init { $v = 10; p.emit("i"); }
    @after { p.emit(`a${$v}`); }
    : A { $v = 1; p.emit("A"); }
    | B { $v = 2; p.emit("B"); }
    ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

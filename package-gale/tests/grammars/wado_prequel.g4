// Source: Gale test fixture (Stage C @init / @after prequels)
// License: same as the Gale package
//
// `@init` runs at rule entry (before the body), `@after` after the body.
// Both share the rule's value channel: @init seeds `$v`, the body updates it,
// @after reads the final value. Emitting in each pins the execution order.
grammar WadoPrequel;

options { language = Wado; }

r returns [i32 v]
    @init { $v = 1; p.emit("i"); }
    @after { p.emit(`a{$v}`); }
    : A { $v = 2; p.emit("b"); } ;

A : 'a' ;
WS : ' ' -> skip ;

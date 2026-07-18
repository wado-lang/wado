// Source: Gale test fixture (Stage C @init / @after on a left-recursive rule)
// License: same as the Gale package
//
// `@init` seeds the invocation's value channel at rule entry (ANTLR's
// enterRecursionRule): the primary alt's action reads the seed (`$v` starts at
// 1000). `@after` runs once at rule exit on the final accumulated value. Each
// recursive `r=e` is an independent invocation, so it runs its own @init/@after.
grammar WadoLrPrequel;

options { language = Wado; }

s : x=e { p.emit(`v=${$x.v} `); } ;

e returns [i32 v]
    @init { $v = 1000; }
    @after { p.emit(`[after=${$v}]`); }
    : l=e '+' r=e { $v = $l.v + $r.v; }
    | n=INT { $v = $n.int + $v; }
    ;

INT : [0-9]+ ;
WS : ' ' -> skip ;

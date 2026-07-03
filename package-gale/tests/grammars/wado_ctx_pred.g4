// Source: Gale test fixture (Stage C context-dependent predicate)
// License: same as the Gale package
//
// An alt-initial predicate that reads the value channel (`$n`, seeded by
// @init). Prediction gates the branch on the translated condition
// (`vals.n == 1`), folded into the token-led dispatch — ANTLR tests the
// predicate even when the alt is otherwise unambiguous.
grammar WadoCtxPred;

options { language = Wado; }

r returns [i32 n]
    @init { $n = 1; }
    : {$n == 1}? A { p.emit(`n={$n} one`); }
    | B { p.emit("b"); }
    ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

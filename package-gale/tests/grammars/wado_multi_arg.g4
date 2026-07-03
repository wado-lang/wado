// Source: Gale test fixture (Stage C multi-alt rule argument + overlap @init)
// License: same as the Gale package
//
// A multi-alt rule argument (`e[i32 base]`) threads through the `_alt_<n>`
// dispatch helpers, and an @init seed reaches an overlapping-alt group too:
// both `A B` alts share their prefix (an overlap group), yet @init's `$acc`
// seed and the arg are visible in the chosen alternative.
grammar WadoMultiArg;

options { language = Wado; }

r : e[100] ;

e[i32 base]
    @init { $acc = $base; }
    returns [i32 acc]
    : A B { p.emit(`sum={$acc}`); $acc = $acc + 1; }
    | A C { p.emit(`alt2 {$acc}`); }
    | D { p.emit(`d {$acc}`); }
    ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
WS : ' ' -> skip ;

// Source: Gale test fixture (a reserve that must not stop a loop with turns left)
// License: same as the Gale package
//
// `(A | B)* A` owes an `A` after the loop. An iteration ending where no `A`
// starts is not a starve while the loop itself can go round again, so the
// reserve breaks only when nothing can follow. On `b b a` the tail cannot start
// after the first `b`, and breaking there loses the input.
grammar ReserveYieldsToAnotherIteration;
start : xs EOF ;
xs : (A | B)* A ;
A : 'a' ;
B : 'b' ;
WS : [ \t\r\n]+ -> skip ;

// Source: Gale test fixture (Stage C: action inside a repeated group)
// License: same as the Gale package
//
// `(A {print})+` collapses the group to its single element at lower time, but
// the alternative's action still belongs inside the loop — once per iteration,
// reading the token that iteration matched.
grammar JavaRepeatAction;

a : (A {System.out.println($A.text);})+ ;

A : [AaBb] ;
WS : (' '|'\n')+ -> skip ;

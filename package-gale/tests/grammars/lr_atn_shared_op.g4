// Two LR operator alternatives that open on the same token, in a rule the
// loop-entry classifier already routes to the simulator.
//
// `'between' expr 'and' expr` is an atom whose inner self-reference is
// followed by `'and'`, the delimiter the binary `expr 'and' expr` alt also
// opens on — the shape `lr_rule_is_atn_class` fires on (cf. lr_between.g4).
// That verdict is whole-rule, so the operator alternatives below run their
// loop decision on the simulator too.
//
// `expr 'not'? 'in' expr` and `expr 'not'? 'like' expr` both admit `'not'`
// at the loop and are told apart only by the token after it. The static LR
// dispatch separates exactly this with a second-token sub-dispatch and a
// scan-twin commit gate; the simulator's loop decision tests one token
// against each continuation's FIRST and returns the first that admits it,
// so `a not like b` is decided by alternative order.

grammar LrAtnSharedOp;

stat : expr ';' ;

expr : ID
     | expr 'and' expr
     | expr 'not'? 'in' expr
     | expr 'not'? 'like' expr
     | 'between' expr 'and' expr
     ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;

// Source: hand-written for Gale's resilient-parser overlapping-LR tests.
// License: same as the Gale package.
//
// The shared first token sits inside a rule that is both multi-alternative and
// multi-token, so neither K-prefix walk can name the second token: the deep one
// halts at the alternation, the shallow one at the width. The dispatch falls
// back to the candidate's own suffix scan, which lets `>` reach `cmp` after
// `op` misses instead of committing to `op` and breaking the loop.
grammar LrOverlapOpaqueOp;

e   : e op e
    | e cmp e
    | INT
    ;

op  : '>' '>'
    | '<' '<'
    ;

cmp : '>' | '<' ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

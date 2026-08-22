// Source: hand-written for Gale's resilient-parser overlapping-LR tests.
// License: same as the Gale package.
//
// The Rust `>>` shape: a shift operator is a two-token rule reference inside
// the shared group, so both LR suffixes start with `>` and only the *second*
// token separates them — which the overlap dispatch can see only by looking
// inside `shr`, past the group alternative's single element.
grammar LrOverlapMultiToken;

e   : e (shl | shr) e
    | e cmp e
    | INT
    ;

shl : '<' '<' ;

shr : '>' '>' ;

cmp : '>' | '<' | '==' ;

INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

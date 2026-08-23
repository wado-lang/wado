// Source: hand-written for Gale's resilient-parser overlapping-LR tests.
// License: same as the Gale package.
//
// The Rust `>>` shape: a shift operator is a two-token rule reference inside
// the shared group, so only the token inside `shr` separates the suffixes.
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

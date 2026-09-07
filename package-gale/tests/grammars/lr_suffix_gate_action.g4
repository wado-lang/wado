// Source: Gale test fixture (a gate action inside a left-recursive suffix)
// License: same as the Gale package
//
// A left-recursive rule's precedence loop *scans* a suffix before committing to
// it, so an action inside the suffix has to run on both sides. Where only the
// parse runs it, the scan measures the suffix under a gate the parse will have
// lifted, declines it, and the loop stops -- the suffix's own tokens are then
// left to whatever follows.
//
// This is `RustParser.g4`'s `expression LSQUAREBRACKET {noStruct = 0;}
// expression RSQUAREBRACKET`: inside an `if` head the index brackets lift the
// struct-literal ban, so `if a[b { c }] { d }` is an index whose subscript is a
// struct literal, and the `{ d }` after it is the block the `if` owes.
grammar LrSuffixGateAction;

@parser::members {
    int noBrace = 0;
}

start
    : stmt+ EOF
    ;

stmt
    : IF {noBrace = 1;} expr {noBrace = 0;} block
    | expr SEMI
    ;

block
    : LB ID RB
    ;

expr
    : expr LSQ {noBrace = 0;} expr RSQ
    | {noBrace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
LSQ  : '[' ;
RSQ  : ']' ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
ID   : [a-z]+ ;
WS   : [ \t\r\n]+ -> skip ;

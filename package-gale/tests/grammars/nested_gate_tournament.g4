// `gated_tournament_alt.g4` covers the gate a parse function evaluates just
// before the tournament it guards. This one covers the position that one does
// not reach: an outer tournament whose scan walks *through* the `IF` head. The
// action closing the gate never runs during that scan, so the scan measures a
// length the parse will not reproduce, and the tournament commits to the
// alternative that measurement favours.
//
// Every part below is load-bearing. `expr` is left-recursive, so its atom
// alternatives are chosen by the scan tournament rather than by lookahead. The
// `EQ` suffix puts the `IF` under that tournament instead of at a statement
// head, where the token alone would decide it. And `stmts` is two alternatives
// sharing every leading token, so the mis-measurement has somewhere to go.
//
// This is `RustParser.g4`'s remaining failure: `fn f() { y = if v { a }; }`,
// where `v { a }` is a longer expression than `v` and only the block the `if`
// still owes separates them.
grammar NestedGateTournament;

@parser::members {
    int noBrace = 0;
}

start
    : block EOF
    ;

block
    : LB stmts RB
    ;

stmts
    : stmt+ expr?
    | expr
    ;

stmt
    : expr SEMI
    ;

blk
    : LB ID RB
    ;

expr
    : expr EQ expr
    | IF {noBrace = 1;} expr {noBrace = 0;} blk
    | {noBrace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
EQ   : '=' ;
ID   : [a-z]+ ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;

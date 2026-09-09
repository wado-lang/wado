// `nested_gate_tournament_wado.g4` with its gate member named the way Wado
// names things.
//
// The two differ only in spelling and share every line of the code that reads
// them, so this catches no bug the camelCase twin does not. It is here because
// `no_brace` is what a Wado grammar will actually be written with, and nothing
// else pins that spelling from the `.g4` through to the tree.
//
// The twin is the stronger regression test of the two: predicting a member's
// name by snake_casing it — which is what left every Wado grammar without a
// gate — still gets `no_brace` right and gets `noBrace` wrong.
grammar NestedGateTournamentWadoSnake;

options {
    language = Wado;
}

@parser::members {
    no_brace: i32 = 0
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
    | IF {p.no_brace = 1} expr {p.no_brace = 0} blk
    | {p.no_brace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
EQ   : '=' ;
ID   : [a-z]+ ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;

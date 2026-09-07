// Source: Gale test fixture (a greedy loop declining an iteration that strands its own alternative)
// License: same as the Gale package
//
// `stmts` is `stmt+ expr?`, and `stmt`'s block form and `expr` both start with
// `{`. A greedy loop takes every iteration its body scans, so `{ y }.m()` loses
// its head to the loop and leaves `.m()` where nothing in the alternative can
// take it -- neither another iteration, nor `expr?`, nor the caller.
//
// The iteration is viable only if something can continue from where it ends.
// Where nothing can, and the continuation *can* consume from where the
// iteration would have started, the loop has to decline it.
//
// This is `RustParser.g4`'s block body: `fn f() { let a = 1; if c {}.m() }`,
// where `if c {}` is a statement everywhere except right here.
grammar LoopDeclinesStrandedIteration;

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
    : LET ID EQ expr SEMI
    | blk SEMI?
    ;

blk
    : LB ID RB
    ;

expr
    : expr DOT ID LPAREN RPAREN
    | blk
    | ID
    ;

LET    : 'let' ;
DOT    : '.' ;
EQ     : '=' ;
SEMI   : ';' ;
LPAREN : '(' ;
RPAREN : ')' ;
LB     : '{' ;
RB     : '}' ;
ID     : [a-z]+ ;
WS     : [ \t\r\n]+ -> skip ;

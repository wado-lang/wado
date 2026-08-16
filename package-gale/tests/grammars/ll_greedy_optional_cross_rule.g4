// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// The same token opens a greedy optional in two rules at once: `('else'
// block)?` ends `ifExpr`, and another `('else' block)?` ends the enclosing
// `stmt`. ANTLR4's greedy subrule takes the innermost, so the `else` is the
// `ifExpr`'s and the expression continues with `/`.
grammar LlGreedyOptionalCrossRule;

prog : stmt EOF ;

stmt : 'let' ID '=' expression ('else' block)? ';' ;

expression
    : expression '/' expression
    | ifExpr
    | INT
    ;

ifExpr : 'if' ID block ('else' block)? ;

block : '{' INT '}' ;

ID  : [a-zA-Z_] [a-zA-Z_0-9]* ;
INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

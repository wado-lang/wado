// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// An optional group whose last element is itself optional. Deciding to enter
// the outer `( ... )?` must not depend on the inner `( ... )?` being taken.
grammar LlNestedOptionalTail;

prog : stmt* EOF ;

stmt : 'let' ID ('=' expr ('else' block)?)? ';' ;

expr
    : expr '+' expr
    | ID '(' ID ')'
    | ID
    | INT
    ;

block : '{' '}' ;

ID  : [a-zA-Z_] [a-zA-Z_0-9]* ;
INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

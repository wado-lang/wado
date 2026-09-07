// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// A group whose second alternative matches empty, at a lookahead neither
// alternative selects. `(COMMA base | COMMA?)` closes `fields`, and at the `}`
// the only reading left is the empty one — reporting "no viable alternative"
// there rejects input the grammar accepts.
//
// `apply_empty_alt_admits` normally answers this by unioning the group's
// FOLLOW into the nullable alternative's admit set, but only where the
// grammar's FOLLOW fixed point is exact. `any` below is what makes it
// inexact here, as the macro token trees do in `RustParser.g4` — which is why
// `Point { x, y }` reached the dispatch at all.
grammar LlNullableAltNoViable;

start
    : (expr | any) EOF
    ;

expr
    : ID LBRACE fields? RBRACE
    ;

fields
    : field (COMMA field)* (COMMA base | COMMA?)
    ;

base
    : DOTDOT ID
    ;

field
    : ID
    ;

any
    : AT .
    ;

ID     : [a-z]+ ;
COMMA  : ',' ;
DOTDOT : '..' ;
LBRACE : '{' ;
RBRACE : '}' ;
AT     : '@' ;
WS     : [ \t\r\n]+ -> skip ;

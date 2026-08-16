// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// Two shapes of one optional whose signatures are in a prefix relation: the
// longer shape truncates at the multi-token `m`, leaving `['=', '@']`, which
// the shorter shape's `['=', '@', '#']` starts with.
grammar LlShapePrefixSignature;

prog : stmt EOF ;

stmt : 'let' ID ('=' m? '@' '#')? ';' ;

m : '@' '!' ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;

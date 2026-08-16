// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// An alternative that starts with a rule reference whose whole body is
// optional. `FIRST(member)` must reach past nullable `mods` to `'fn'`, or the
// `member*` loop never enters on a bare `fn`.
grammar LlNullableRuleHead;

prog : member* EOF ;

member
    : 'type' ID ';'
    | fnDecl
    ;

fnDecl : mods 'fn' ID ';' ;

mods : 'pub'? ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;

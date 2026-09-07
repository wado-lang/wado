// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// The same loop as `ll_loop_yields_closer.g4`, with the closer inside a group:
// `block : (OR names? OR) body`. What continues past `names` is the group's
// own OR and then `body`, which sits in the enclosing alternative — a
// continuation the group's element list does not mention.
//
// This is `RustParser.g4`'s `closureExpression`, whose parameter list lives in
// `(OROR | OR closureParameters? OR)` and whose body follows that group. It is
// why a closure still fails as a call argument (`g(|e| e)`) after the flat
// shape parses.
grammar LlGroupTailOuterSuffix;

start
    : block EOF
    ;

block
    : (OR names? OR) body
    ;

names
    : name (COMMA name)*
    ;

name
    : ID (OR ID)*
    ;

body
    : ID
    ;

ID    : [a-z]+ ;
OR    : '|' ;
COMMA : ',' ;
WS    : [ \t\r\n]+ -> skip ;

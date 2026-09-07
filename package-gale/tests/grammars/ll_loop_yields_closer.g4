// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// A tail-greedy loop on the very token its caller closes with. `name`'s
// `(OR ID)*` sits inside `block : OR names? OR ID`, so the `|` after the first
// name is the closer, not another iteration — but the loop sees the same token
// either way, and only the caller's continuation says which.
//
// This is `RustParser.g4`'s or-pattern (`pattern : ... (OR patternNoTopAlt)*`)
// inside `closureExpression`'s `OR closureParameters? OR`, where it costs every
// closure that names a parameter (`|e| e.to_string()`).
grammar LlLoopYieldsCloser;

start
    : block EOF
    ;

block
    : OR names? OR ID
    ;

names
    : name (COMMA name)*
    ;

name
    : ID (OR ID)*
    ;

ID    : [a-z]+ ;
OR    : '|' ;
COMMA : ',' ;
WS    : [ \t\r\n]+ -> skip ;

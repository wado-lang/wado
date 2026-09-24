// A nested optional whose body starts on its own continuation's token: `m?`
// before `'@' '#'`, with `m : '@' '!'`. One token of lookahead cannot tell
// `m` from the `'@'` after it, so the optional is decided by scanning `m` and
// then the rest of the group. Each alternative reaches that `m?` by another
// route: behind a fixed-token optional entry, as a shape of an optional with
// none, inside a mandatory group, and as a one-token body (`'@'?`).
grammar LlShapePrefixSignature;

prog : stmt EOF ;

stmt : 'let' ID ('=' m? '@' '#')? ';'
     | 'var' ID (m? '@' '#')? ';'
     | 'put' ID ('=' m? '@' '#') ';'
     | 'tok' ID ('=' '@'? '@' '#')? ';'
     ;

m : '@' '!' ;

ID : [a-zA-Z_] [a-zA-Z_0-9]* ;
WS : [ \t\r\n]+ -> skip ;

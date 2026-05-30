// Regression: two alts share an IDENT prefix and tie on static length
// (`'mut'? IDENT` vs `path '(' ... ')'`). For `N(n)` the scanner must
// pick the longer variant alt, not commit to the bare-IDENT one.
grammar alt_shared_ident_prefix;

prog
    : matchExpr EOF
    ;

matchExpr
    : 'match' IDENT '{' (arm (',' arm)* ','?)? '}'
    ;

arm
    : pattern '=>' IDENT
    ;

pattern
    : '_'
    | 'mut'? IDENT
    | path ('(' (pattern (',' pattern)*)? ')')?
    | '(' (pattern (',' pattern)*)? ')'
    ;

path
    : IDENT ('::' IDENT)*
    ;

IDENT
    : [a-zA-Z_] [a-zA-Z0-9_]*
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

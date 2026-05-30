// Regression: optional separated list with a trailing separator —
// `( arm (',' arm)* ','? )?`. The Star's ',' collides with the trailing
// ','?, so shape-lookahead must fall back to a scan guard. `prog`
// exercises the parse side; `wrapped` (via a tournament) the scan side.
grammar opt_sep_list;

prog
    : list EOF
    ;

list
    : (arm (',' arm)* ','?)?
    ;

stmt
    : wrapped EOF
    ;

wrapped
    : braced
    | arm
    ;

braced
    : '{' (arm (',' arm)* ','?)? '}'
    ;

arm
    : IDENT
    ;

IDENT
    : [a-zA-Z_] [a-zA-Z0-9_]*
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

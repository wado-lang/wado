// Regression: an optional group whose body is a separated list with a
// trailing separator — `( arm (',' arm)* ','? )?`. The inner Star
// `(',' arm)*` shares its first token (',') with the trailing `','?`,
// so fixed-length shape-lookahead cannot distinguish "more arms" from
// "trailing comma". Gale must fall back to a scan-guarded body so
// `a , b` keeps consuming arms instead of mis-committing to the
// trailing-comma shape (which left `b` unparsed → "expected EOF").
//
// `prog` exercises the parse-side lowering of the optional list.
// `wrapped` puts the same optional list inside a `{ ... }` braced form
// reached through an alternative tournament, so the scan-side lowering
// (`scan_wrapped`) is exercised at runtime too — the shape that, before
// the fix, generated an out-of-bounds-prone duplicate-guard scanner.
//
// Original work, written as a Gale regression fixture.
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

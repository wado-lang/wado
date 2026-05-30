// Regression: a rule with two alternatives that share an `IDENT`
// leading prefix, where the *longer* alt continues past the bare
// identifier — `'mut'? IDENT` vs `path '(' ... ')'`. For input `N(n)`
// the scanner must pick the longer `path '(' pattern ')'` alt, not
// commit to the bare-`IDENT` alt and leave `( n )` unconsumed (which
// surfaced as "expected RBRACE, got N" when such a pattern appeared in
// a match arm).
//
// This mirrors the `pattern` rule of a real language grammar, where a
// bare identifier is a binding and `Name(sub)` is a variant pattern.
//
// Original work, written as a Gale regression fixture.
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

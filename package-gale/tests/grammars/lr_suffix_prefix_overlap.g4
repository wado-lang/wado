// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// Two left-recursive suffixes where one is the other's prefix: `e DOT ID LP RP`
// against `e DOT ID`. Neither the leading DOT nor the ID that follows it picks
// one — only the token past the ID does, and for the shorter suffix that token
// belongs to whatever encloses the expression.
//
// This is `RustParser.g4`'s `MethodCallExpression` against `FieldExpression`,
// where it costs every field access (`self.x`).
grammar LrSuffixPrefixOverlap;

start
    : e EOF
    ;

e
    : e DOT ID LPAREN RPAREN # Call
    | e DOT ID               # Field
    | ID                     # Var
    ;

ID     : [a-z]+ ;
DOT    : '.' ;
LPAREN : '(' ;
RPAREN : ')' ;
WS     : [ \t\r\n]+ -> skip ;

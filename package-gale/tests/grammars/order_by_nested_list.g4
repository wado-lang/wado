// Source: https://github.com/antlr/antlr4/issues/4890 (the parser rules; `s`,
// `KEY`, the parentheses and `WS` are written here).
// License: none stated in the report.
//
// A list rule that recurses into itself inside its own loop,
// `orderBy : clause | clause (',' orderBy)*`, anchored by EOF. Anything left
// after the last clause must be rejected rather than dropped.
grammar OrderByNestedList;

s : orderBy EOF ;

orderBy : orderByClause | orderByClause (',' orderBy)* ;
orderByClause : path DIRECTION ;
path : expr ;
expr : atom (('+' | '-') atom)* ;
atom : KEY ('.' KEY)* | LPAREN expr RPAREN ;

DIRECTION : 'ASC' | 'DESC' ;
KEY : [a-z]+ ;
LPAREN : '(' ;
RPAREN : ')' ;
WS : [ \t\r\n]+ -> skip ;

// Source: https://github.com/antlr/antlr4/issues/4911
// License: none stated in the report.
//
// An atom alternative whose closing token is optional,
// `'(' expr ')'?`, ends in something that can match nothing. It must still be
// an atom and not a prefix operator, so `.` keeps binding tighter than `==`.
grammar LrOptionalCloseAtom;

start : (expr ';')* EOF ;

expr
  : Identifier
  | '(' expr ')'?
  | expr '.' Identifier
  | expr '==' expr
  ;

Identifier : [a-z]+ ;
Ws : [ \t\n\r]+ -> skip ;

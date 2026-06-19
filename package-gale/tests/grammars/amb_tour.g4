// Source: hand-written for Gale's resilient-parser tournament tests.
// License: same as the Gale package.
//
// `stat`'s two alternatives share the prefix `ID`, so the choice needs a
// longest-match scan tournament: `ID '=' expr` wins when `=` follows, else the
// bare `expr` alternative.
grammar AmbTour;

prog : stat EOF ;

stat : ID '=' expr   # Assign
     | expr          # Bare
     ;

expr : ID | INT ;

ID  : [a-z]+ ;
INT : [0-9]+ ;
WS  : [ \t]+ -> skip ;

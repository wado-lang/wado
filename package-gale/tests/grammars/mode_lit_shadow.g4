// Source: hand-written regression for the mode / inline-literal shadow.
// License: project-internal test fixture.
// `{` is an inline literal in `block`, so it is a parser literal token. A `{`
// rule (INTERP_OPEN) also lives in the STR mode; it must keep its matcher —
// inline literals are only matched in the default mode, so shadowing it would
// leave STR with no `{` and the interpolation would never open.
grammar ModeLitShadow;
prog   : block tstr EOF ;
block  : '{' '}' ;
tstr   : BACKTICK interp BACKTICK ;
interp : INTERP_OPEN WORD '}' ;

BACKTICK : '`' -> pushMode(STR) ;
WORD     : [a-z]+ ;
LBRACE   : '{' -> pushMode(DEFAULT_MODE) ;
RBRACE   : '}' -> popMode ;
WS       : [ \t\r\n]+ -> skip ;

mode STR;
INTERP_OPEN : '{' -> pushMode(DEFAULT_MODE) ;
STR_END     : '`' -> type(BACKTICK), popMode ;

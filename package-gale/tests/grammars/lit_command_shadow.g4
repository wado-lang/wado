// Source: hand-written regression for the inline-literal / named-rule shadow.
// License: project-internal test fixture.
// `{` is written inline in a parser rule and also has a named lexer rule that
// carries a mode-push command. ANTLR unifies the two, so the command must fire:
// WORD is only lexable in the pushed mode, so a dropped command leaves it
// unrecognized.
grammar LitCommandShadow;
prog   : '{' WORD EOF ;
LBRACE : '{' -> pushMode(INNER) ;
WS     : [ \t\r\n]+ -> skip ;
mode INNER;
WORD : [a-z]+ ;

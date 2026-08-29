// Source: Gale test fixture (Stage C surface matrix)
// License: same as the Gale package
//
// Elements named after the locals a generated parse function binds. Each one
// snake_cases onto a name the emitted body already reads — the handle `p`, the
// value channel `vals`, the multi-alt dispatch's lookahead `kind`, the FOLLOW
// mask, an LR rule's precedence — so an unescaped binding redeclares it in the
// same scope and the rest of the alternative reads a token where it meant the
// parser. The assertion is that this compiles and parses; `escape_ident` is
// what makes it.
grammar SurfaceNames;

options { language = Wado; }

prog : one | two ;

one : P KIND { p.emit("one") } ;
two : VALS FOLLOW MIN_PREC { p.emit("two") } ;

P : 'p' ;
KIND : 'k' ;
VALS : 'v' ;
FOLLOW : 'f' ;
MIN_PREC : 'm' ;
WS : [ \t\r\n]+ -> skip ;

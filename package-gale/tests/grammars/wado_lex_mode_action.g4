// Source: Gale test fixture (Stage C lexer mode-op actions)
// License: same as the Gale package
//
// `language = Wado` lexer `{ ... }` actions that switch lexer mode:
// `lx.push_mode(MODE_INNER)` and `lx.pop_mode()`. Letters lex as `W` in the
// default mode; inside `< ... >` the lexer is in `INNER`, where digits lex as
// `N`. The emitted token sequence proves the mode stack moved. Mode names are
// emitted as `MODE_<NAME>` constants. The Wado analog of ANTLR's lexer
// `pushMode` / `popMode`.
grammar WadoLexModeAction;

options { language = Wado; }

prog : (item)* EOF ;
item : W { p.emit("w") }
     | N { p.emit("n") }
     | LT { p.emit("<") }
     | GT { p.emit(">") }
     ;

LT : '<' { lx.push_mode(MODE_INNER) } ;
W : [a-z]+ ;
WS : ' ' -> skip ;

mode INNER;
GT : '>' { lx.pop_mode() } ;
N : [0-9]+ ;

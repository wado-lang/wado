// Source: Gale test fixture (Stage C lexer mode-op actions)
// License: same as the Gale package
//
// `language = Wado` lexer `{ ... }` actions that switch lexer mode:
// `lx.push_mode(MODE_INNER)` and `lx.pop_mode()`, plus the `$mode` attribute,
// which reads the current mode and assigns a switch to it. Letters lex as `W`
// in the default mode; inside `< ... >` the lexer is in `INNER`, where digits
// lex as `N`. The emitted token sequence proves the mode stack moved. Mode
// names are emitted as `MODE_<NAME>` constants. The Wado analog of ANTLR's
// lexer `pushMode` / `popMode` / `_mode`.
grammar WadoLexModeAction;

options { language = Wado; }

prog : (item)* EOF ;
item : W { p.emit("w") }
     | N { p.emit("n") }
     | LT { p.emit("<") }
     | GT { p.emit(">") }
     | M { p.emit("m") }
     ;

LT : '<' { lx.push_mode(MODE_INNER) } ;
// `$mode` reads the mode this token is being matched in. The switch it writes
// is the pending command the commit applies, so reading that one would answer
// "no switch issued" rather than the current mode, and the guard would never
// hold.
M : 'm' { if $mode == MODE_DEFAULT_MODE { $mode = MODE_INNER } } ;
W : [a-z]+ ;
WS : ' ' -> skip ;

mode INNER;
GT : '>' { lx.pop_mode() } ;
N : [0-9]+ ;

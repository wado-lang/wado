// Source: Gale test fixture (Stage C lexer-action best_act regression)
// License: same as the Gale package
//
// Regression for a winner-replay bug: `SHORT` (an action rule) and `LONG` (no
// action) share a first char, so both run in one dispatch group. On "ab",
// `SHORT` matches "a" first (recording its replay index), then `LONG` matches
// the longer "ab" and wins. Every winner block must reset the replay index, or
// `LONG`'s token would wrongly replay `SHORT`'s `lx.set_type`. Char-class rules
// (not single literals) keep both in the tournament rather than the keyword
// path. The parser reports the kind that actually arrived.
grammar WadoLexActionStale;

options { language = Wado; }

tokens { RETYPED }

prog : (item)* EOF ;
item : LONG { p.emit("L") }
     | SHORT { p.emit("S") }
     | RETYPED { p.emit("R") }
     ;

SHORT : [a-z] { lx.set_type(TK_RETYPED) } ;
LONG : [a-z] [a-z] ;
WS : ' ' -> skip ;

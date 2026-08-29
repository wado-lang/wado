// Source: Gale test fixture (Stage C lexer $-attribute surface)
// License: same as the Gale package
//
// ANTLR's lexer attributes in a `language = Wado` body. `$type` is the
// assignable token type — writing it is exactly what `lx.set_type(...)` does —
// and `$text` / `$index` / `$pos` read the match window: the matched slice, the
// cursor's char index, and its column within the line.
grammar WadoLexAttrs;

options { language = Wado; }

s : t+ ;

t : A { p.emit("a") }
  | B { p.emit("b") }
  | D { p.emit("d") }
  | P { p.emit("p") }
  | R { p.emit("r") }
  ;

A : 'a'+ { if $index - start == 2 { $type = TK_B } } ;
D : 'd'+ { if $text == "dd" { $type = TK_B } } ;
P : 'p'+ { if $pos == 2 { $type = TK_B } } ;
// Reading `$type` answers what the tournament settled on before any action ran.
R : 'r' { if $type == TK_R { $type = TK_B } } ;
B : 'b' ;
WS : [ \n] -> skip ;

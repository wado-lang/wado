// Source: Gale test fixture (Stage C $start / $stop / multi-alt $text)
// License: same as the Gale package
//
// `$start` / `$stop` are the rule's first / last token (member-addressable like
// any token), and `$text` is the rule's consumed input — all from the captured
// first-token index. Exercised on a multi-alt rule so the span capture is
// emitted per dispatch shape, not just single-alt.
grammar WadoStartStop;

options { language = Wado; }

r : A B C { p.emit(`text=${$text} start=${$start.text} stop=${$stop.text}`); }
  | D { p.emit(`d text=${$text} start=${$start.text}`); }
  ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
WS : ' ' -> skip ;

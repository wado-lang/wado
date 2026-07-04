// Source: Gale test fixture (Stage C lexer semantic predicates)
// License: same as the Gale package
//
// A trailing lexer predicate gates whether a rule wins: `KW` matches a run of
// letters only when exactly four were consumed (`pos - start == 4`, with the
// match fn's `chars` / `start` / `pos` in scope). A four-letter word lexes as
// KW; any other length falls through to ID. `DISABLED` is unreachable via a
// `{ false }?` predicate.
grammar WadoLexPred;

options { language = Wado; }

s : k=KW { p.emit("kw"); }
  | i=ID { p.emit("id"); }
  ;

DISABLED : 'kw' { false }? ;
KW : [a-z]+ { pos - start == 4 }? ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

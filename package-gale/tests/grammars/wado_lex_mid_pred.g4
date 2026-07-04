// Source: Gale test fixture (Stage C mid-alt lexer semantic predicate)
// License: same as the Gale package
//
// A predicate sits *between* elements of one alt (not at a boundary): after
// `KW`'s first letter and before the rest, `{ $text == "c" }?` requires that
// first letter to be `c`. So a three-letter run lexes as KW only when it starts
// with `c`, otherwise it falls through to ID. Exercises mid-alt predicate
// placement in `gen_lexer_alt_seq`.
grammar WadoLexMidPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | i=ID { p.emit("id") }
  ;

KW : [a-z] { $text == "c" }? [a-z] [a-z] ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

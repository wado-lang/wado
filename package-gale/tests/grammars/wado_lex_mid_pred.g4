// Source: Gale test fixture (Stage C mid-alt lexer semantic predicate)
// License: same as the Gale package
//
// A predicate sits *between* elements of one alt (not at a boundary): after
// `KW`'s first letter and before the rest, `{ $text == "c" }?` requires that
// first letter to be `c`. So a three-letter run lexes as KW only when it starts
// with `c`, otherwise it falls through to ID. Exercises mid-alt predicate
// placement in `gen_lexer_alt_seq`.
//
// `NG` additionally places a predicate *after a non-greedy repeat*
// (`'q' [a-z]*? { pos - start == 2 }? 'z'`): the predicate must still be emitted
// (into the synthesized non-greedy suffix), so `qz` (one letter after `q`) is
// NG but `qz` with zero letters is not — it falls through to ID.
grammar WadoLexMidPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | n=NG { p.emit("ng") }
  | i=ID { p.emit("id") }
  ;

KW : [a-z] { $text == "c" }? [a-z] [a-z] ;
NG : 'q' [a-z]*? { pos - start == 2 }? 'z' ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

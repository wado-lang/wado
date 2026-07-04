// Source: Gale test fixture (Stage C boundary predicate on a single-alt fragment)
// License: same as the Gale package
//
// A trailing predicate on a *single-alt* fragment inlined at its use site:
// `KW` references `LETTERS`, whose one alt matches a letter run gated by
// `{ $text == "cat" }?`. Unlike a multi-alt fragment (which goes through
// `gen_lexer_alts`), a single-alt fragment is emitted straight through
// `gen_lexer_alt_seq`, so its boundary (`before_index == 0` / `== len`)
// predicates must be placed by `gen_lexer_alt_seq` itself. `cat` lexes as KW,
// other runs fall through to ID.
grammar WadoLexFragBoundaryPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | i=ID { p.emit("id") }
  ;

KW : LETTERS ;
fragment LETTERS : [a-z]+ { $text == "cat" }? ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

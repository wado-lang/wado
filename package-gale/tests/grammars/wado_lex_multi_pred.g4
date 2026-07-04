// Source: Gale test fixture (Stage C multi-alt lexer semantic predicate)
// License: same as the Gale package
//
// A multi-alt lexer rule carries a per-alt predicate: `KW`'s first alt matches
// a letter run only when it spells `cat`, its second alt matches the literal
// `dog`. A false predicate on the first alt falls through to the next alt (and,
// if none match, to the next rule). So `cat`/`dog` lex as KW and any other run
// falls through to ID. Exercises `gen_lexer_alts` predicate threading.
grammar WadoLexMultiPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | i=ID { p.emit("id") }
  ;

KW : [a-z]+ { $text == "cat" }?
   | 'dog'
   ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

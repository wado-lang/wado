// Source: Gale test fixture (Stage C fragment-inlined lexer semantic predicate)
// License: same as the Gale package
//
// A predicate lives inside a (non-recursive, multi-alt) fragment that is
// inlined at its use site: `KW` references `LETTERS`, whose first alt matches a
// letter run only when it spells `cat` and whose second matches `dog`. `$text`
// reads the enclosing rule's matched slice. `cat`/`dog` lex as KW, other runs
// fall through to ID. Exercises the `gen_lexer_elem` fragment ->
// `gen_lexer_alts` predicate path.
grammar WadoLexFragPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | i=ID { p.emit("id") }
  ;

KW : LETTERS ;
fragment LETTERS : [a-z]+ { $text == "cat" }? | 'dog' ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

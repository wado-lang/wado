// Source: Gale test fixture (Stage C group-inlined lexer semantic predicate)
// License: same as the Gale package
//
// A predicate lives inside a group (not at the rule's alt boundary): `KW`'s
// single alt is a group whose first branch matches a letter run only when it
// spells `cat`, whose second branch matches the literal `dog`. A false
// predicate falls through to the next group branch, then the next rule. So
// `cat`/`dog` lex as KW and any other run falls through to ID. Exercises the
// `gen_lexer_elem` Group -> `gen_lexer_alts` predicate path.
grammar WadoLexGroupPred;

options { language = Wado; }

s : k=KW { p.emit("kw") }
  | i=ID { p.emit("id") }
  ;

KW : ([a-z]+ { $text == "cat" }? | 'dog') ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

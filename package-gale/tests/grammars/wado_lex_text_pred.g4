// Source: Gale test fixture (Stage C lexer `$text` predicate)
// License: same as the Gale package
//
// The `language = Wado` analog of ANTLR's `SemPredEvalLexer/EnumNotID`
// (`ENUM : [a-z]+ { getText().equals("enum") }? ;`): a trailing lexer
// predicate reads `$text` — the matched slice so far — so `ENUM` wins the
// longest-match tie against `ID` only when the run of letters spells `enum`.
// `$text` resolves to `LexerSlice::new(chars, start, pos).to_string()`.
grammar WadoLexTextPred;

options { language = Wado; }

s : e=ENUM { p.emit("enum") }
  | i=ID { p.emit("id") }
  ;

ENUM : [a-z]+ { $text == "enum" }? ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

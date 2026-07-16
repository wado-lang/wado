// Source: Gale test fixture (Stage C lexer more action)
// License: same as the Gale package
//
// A `language = Wado` lexer `{ lx.more() }` action: the `A` rule matches 'a'
// but `more()` suppresses emission and folds the matched text into the next
// token, so `ab` lexes as a single `B` whose text is "ab" (ANTLR's lexer
// `more`). The parser reads the `B` token's text to observe the accumulation.
grammar WadoLexMoreAction;

options { language = Wado; }

s : t=B { p.emit($t.text) } ;

A : 'a' { lx.more() } ;
B : 'b' ;

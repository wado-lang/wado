// Source: Gale test fixture (Stage C: java2wado over lexer bodies)
// License: same as the Gale package
//
// A `language = Java` lexer (ANTLR4's default, so no `options` block): the
// action bodies must run through java2wado, `System.out.println(...)` must
// reach a lexer-side output sink, and `getText()` must resolve to the matched
// text in both an action and a predicate.
lexer grammar JavaLexPrint;

I : '0'..'9'+ {System.out.println("I");} ;
ENUM : [a-z]+ { getText().equals("enum") }? { System.out.println("enum!"); } ;
ID : [a-z]+ {System.out.println("ID " + getText());} ;
WS : (' '|'\n') -> skip ;

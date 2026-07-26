// Source: Gale test fixture (Stage C: java2wado over lexer bodies)
// License: same as the Gale package
//
// A `language = Java` lexer — ANTLR4's default, so no `options` block. Pins
// that java2wado runs the bodies, that `System.out.println` reaches the lexer
// sink, and that `getText()` resolves in an action and in a predicate.
lexer grammar JavaLexPrint;

I : '0'..'9'+ {System.out.println("I");} ;
ENUM : [a-z]+ { getText().equals("enum") }? { System.out.println("enum!"); } ;
ID : [a-z]+ {System.out.println("ID " + getText());} ;
WS : (' '|'\n') -> skip ;

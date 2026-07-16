// Source: Gale test fixture (Stage C lexer `set_type` action)
// License: same as the Gale package
//
// A `language = Wado` lexer `{ ... }` action that retypes the emitted token.
// `A` matches 'a' but its action calls `lx.set_type(TK_B)`, so the token the
// parser sees is a `B`. The action runs only for the winning rule, in the
// tournament commit — the Wado analog of ANTLR's lexer `setType` (`SetType`
// descriptors). The parser rule reports which token kind actually arrived.
grammar WadoLexSetType;

options { language = Wado; }

s : B { p.emit("B") }
  | A { p.emit("A") }
  ;

A : 'a' { lx.set_type(TK_B) } ;
B : 'b' ;
WS : ' ' -> skip ;

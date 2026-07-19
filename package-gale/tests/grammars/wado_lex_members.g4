// Source: Gale test fixture (Stage C @lexer::members)
// License: same as the Gale package
//
// A `language = Wado` lexer with a persistent `@lexer::members` counter field.
// Rule A's action bumps the counter and retypes every 2nd match to B, proving
// the field lives on the `Lexer` (not the per-token `LexerActions`) and survives
// across tokens. The action reaches the field through the `lexer` handle the
// apply fn receives, and the effect channel `lx` unchanged.
grammar WadoLexMembers;

options { language = Wado; }

@lexer::members {
    count: i32 = 0
}

s : t* ;
t : A { p.emit("A") }
  | B { p.emit("B") }
  ;

A : 'a' { lexer.count += 1; if lexer.count % 2 == 0 { lx.set_type(TK_B) } } ;
B : 'b' ;
WS : ' ' -> skip ;

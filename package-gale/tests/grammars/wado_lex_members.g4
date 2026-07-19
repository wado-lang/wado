// Source: Gale test fixture (Stage C @lexer::members)
// License: same as the Gale package
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

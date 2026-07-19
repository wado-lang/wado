// Source: Gale test fixture (Stage C @lexer::members predicate read)
// License: same as the Gale package
grammar WadoLexMemberPred;

options { language = Wado; }

@lexer::members {
    seen: i32 = 0
}

s : t* ;
t : A { p.emit("A") }
  | B { p.emit("B") }
  ;

A : { lx.seen < 2 }? 'a' { lx.seen += 1 } ;
B : 'a' ;
WS : ' ' -> skip ;

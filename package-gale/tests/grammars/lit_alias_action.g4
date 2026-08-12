// Source: Gale test fixture (issue #1752 — an aliased rule keeps its action)
// License: same as the Gale package
//
// `'kw'` is an alias for `K`, so `K`'s action must still run. The counter
// makes that observable: the second `kw` retypes to `X`.
grammar LitAliasAction;

options { language = Wado; }

@lexer::members {
    count: i32 = 0
}

s : t* ;
t : 'kw' { p.emit("KW") }
  | X    { p.emit("X") }
  ;

K  : 'kw' { lx.count += 1; if lx.count % 2 == 0 { lx.set_type(TK_X) } } ;
X  : 'x' ;
WS : ' ' -> skip ;

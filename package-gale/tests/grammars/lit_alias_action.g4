// Source: Gale test fixture (issue #1752 — an aliased rule keeps its action)
// License: same as the Gale package
//
// `K`'s whole body is `'kw'`, which the parser also writes inline, so the
// literal is an alias for `K`. `try_extract_fixed_text` ignores actions, so
// refusing the alias here would dedup `K` onto the shared literal matcher and
// drop the action with it. The counter makes that observable: the second `kw`
// retypes to `X`.
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

// Source: Gale test fixture (Stage C token member access beyond .text)
// License: same as the Gale package
//
// Token members other than `.text`: `$x.int` parses the lexeme as an integer
// (i64), `$x.index` is the token's stream index. Both read the token index the
// call site's `let x = p.expect(...)` bound, through the context API.
grammar WadoTokMembers;

options { language = Wado; }

r : a=NUM b=NUM { p.emit(`${$a.int}+${$b.int}=${$a.int + $b.int} idx=${$b.index}`); } ;

NUM : [0-9]+ ;
WS : ' ' -> skip ;

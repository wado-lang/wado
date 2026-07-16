// Source: Gale test fixture (Stage C lexer skip / set_channel actions)
// License: same as the Gale package
//
// `language = Wado` lexer `{ ... }` actions that route the matched token away
// from the parser: `lx.skip()` drops it, `lx.set_channel(1)` moves it to a
// hidden channel. Both run at the tournament commit for the winning rule. The
// parser rule matches only the visible `B`, so a clean parse proves the `a`
// and `c` tokens never reached the parser stream.
grammar WadoLexSkipAction;

options { language = Wado; }

s : B { p.emit("B") } ;

SK : 'a' { lx.skip() } ;
CH : 'c' { lx.set_channel(1) } ;
B : 'b' ;
WS : ' ' -> skip ;

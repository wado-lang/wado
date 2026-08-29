// Source: Gale test fixture (Stage C same-named label over two rules)
// License: same as the Gale package
//
// One label name bound to a *different* rule in each alternative. Per-alt
// resolution has to pick the rule the enclosing alternative actually called —
// `$x.n` is `a`'s returns field and `$x.m` is `b`'s, and neither rule has the
// other's — so reading the first-declared binding resolves the wrong channel.
grammar WadoLabelCrossRule;

options { language = Wado; }

r : x=a { p.emit(`${$x.n}`); }
  | x=b { p.emit(`${$x.m}`); }
  ;

a returns [i32 n] : A { $n = 1; } ;

b returns [i32 m] : B { $m = 2; } ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

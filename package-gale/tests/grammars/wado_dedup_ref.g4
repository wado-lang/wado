// Source: Gale test fixture (Stage C deduped cross-rule value reference)
// License: same as the Gale package
//
// A token `E` and a rule `e` derive the same field base `e`, so the second
// (the rule call) is deduped to `e_2`. `$e.v` must read that actual binding,
// not the token `E`'s `e` — the translator uses the emit's post-dedup binding.
grammar WadoDedupRef;

options { language = Wado; }

r : E e { p.emit(`v={$e.v}`); } ;

e returns [i32 v] : N { $v = 7; } ;

E : 'e' ;
N : 'n' ;
WS : ' ' -> skip ;

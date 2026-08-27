// Source: Gale test fixture (Stage C group-scoped actions / predicates)
// License: same as the Gale package
//
// An action written in a group's alternative belongs to that alternative: it
// runs when that alternative is taken, and once per iteration under a repeat.
// A predicate at a group alternative's head selects the alternative the way one
// at a rule alternative's head does — `gated` takes its branch only while the
// predicate holds, so the repeat exits into the trailing `B` instead of eating
// it.
grammar WadoGroupAction;

options { language = Wado; }

alts : ( A { p.emit("a") } | B { p.emit("b") } )+ EOF ;

one : ( A B { p.emit("ab") } ) EOF ;

opt : ( A { p.emit("opt") } )? B EOF ;

gated : ( A { p.emit("in") } | { p.la(2) == TK_B }? B { p.emit("gated") } )* B { p.emit("out") } EOF ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

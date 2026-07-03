// Source: Gale test fixture (Stage C predicate-gated prediction)
// License: same as the Gale package
//
// Two alts share the lookahead prefix `A B`; their alt-initial semantic
// predicates decide which one prediction takes. `{false}?` disables alt 0 and
// `{true}?` selects alt 1, so the parser must emit "alt1", not the
// grammar-order-first "alt0". A third alt on a distinct token stays a plain
// token-led branch.
grammar WadoPredSelect;

options { language = Wado; }

item : {false}? A B { p.emit("alt0"); }
     | {true}?  A B { p.emit("alt1"); }
     | C D { p.emit("alt2"); }
     ;

// A predicated alt sharing a prefix with an unpredicated one: when the
// predicate fails, the unpredicated alt is the always-viable fallback.
fallback : {false}? A B { p.emit("fb0"); }
         | A B { p.emit("fb1"); }
         ;

A : 'a' ;
B : 'b' ;
C : 'c' ;
D : 'd' ;
WS : ' ' -> skip ;

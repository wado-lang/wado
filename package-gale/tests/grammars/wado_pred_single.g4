// Source: Gale test fixture (Stage C predicate-gated single-token prediction)
// License: same as the Gale package
//
// The canonical `SemPredEvalParser/Simple` shape in Wado: every alternative is
// a single token, so the compact single-token fast path would normally run no
// actions or predicates. With actions present the rule must take the general
// multi-alt path: alts 0 and 1 tie on `A`, and their alt-initial predicates
// select alt 1; alt 2 is a distinct token.
grammar WadoPredSingle;

options { language = Wado; }

item : {false}? A { p.emit("alt0"); }
     | {true}?  A { p.emit("alt1"); }
     | B { p.emit("alt2"); }
     ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

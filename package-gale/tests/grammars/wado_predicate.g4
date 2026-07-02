// Source: Gale test fixture (Stage C semantic predicates)
// License: same as the Gale package
//
// A mid-alt semantic predicate `{cond}?` gates the parse: when false during a
// real parse the rule fails. `ok` reads a token then a true predicate lets it
// finish; `bad` uses a false predicate so the parse never completes.
grammar WadoPredicate;

options { language = Wado; }

ok : A { p.emit("go"); } {true}? B ;
bad : A {false}? B ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;

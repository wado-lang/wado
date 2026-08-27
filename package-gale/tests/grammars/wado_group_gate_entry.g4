// Source: Gale test fixture (a group's entry test vs its dispatch)
// License: same as the Gale package
//
// A construct deciding whether to enter a group tests first sets; the dispatch
// inside also tests the alternatives' predicates. Both must agree, or a token
// only a gated alternative could take is admitted and then has no arm.
grammar GroupGateEntry;

options { language = Wado; }

// The optional matches nothing and `A` takes the token.
opt : ( { false }? A | B )? A ;

// The loop exits rather than reporting.
star : ( { false }? A | B )* A ;

nongreedy : ( { false }? A | B )*? A ;

list_label : xs += ( { false }? A | B )* A ;

// A predicate inside a single-alternative group guards; it selects nothing,
// even where the enclosing rule has alternatives of its own.
nested : ( A ( { false }? B ) | C ) ;

// A single-alternative group carries the loop's own entry decision, so its
// predicate gates the loop rather than failing inside it.
solo_star : ( { false }? A B )* A ;

solo_opt : ( { false }? A )? A ;

// An optional whose entry is decided by enumerating its shapes: the gate joins
// every shape's condition, as it joins a first-set one. A leading rule
// reference keeps the k-token lookahead from deciding it first.
shape_opt : ( { false }? ra B? A )? A EOF | C EOF ;

ra : A ;

A : 'a' ;

B : 'b' ;

C : 'c' ;

WS : ' ' -> skip ;

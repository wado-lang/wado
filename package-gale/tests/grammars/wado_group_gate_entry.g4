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

A : 'a' ;

B : 'b' ;

C : 'c' ;

WS : ' ' -> skip ;

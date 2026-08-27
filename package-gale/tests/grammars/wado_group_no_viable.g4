// Source: Gale test fixture (a required group that takes no alternative)
// License: same as the Gale package
//
// A required group that takes none of its alternatives: ANTLR4 reports a
// no-viable-alternative there, one shape per group class.
grammar GroupNoViable;

options { language = Wado; }

// Token-only alternatives, one carrying an action.
tok : ( A { p.emit("a") } | B ) C EOF ;

// An alt-initial predicate that never holds.
gated : ( { false }? A { p.emit("x") } | B ) A EOF ;

// A `+`'s mandatory first iteration is a required position: the loop gate must
// not exit it, so a token no alternative admits is reported there.
plus_gated : ( { false }? A { p.emit("x") } | B )+ A EOF ;

// One alternative, so there is no dispatch to report from: the loop gate is the
// only place the predicate is answered, and the first iteration runs past it.
plus_solo : ( { false }? A B )+ A EOF ;

// One overlap group, so every branch of the dispatch is conditional and none
// takes the rest. The loop's scan guard covers the iterations after the first;
// the mandatory one has nothing to answer with.
plus_scan : ( A B | A C )+ C EOF ;

// Rule-reference alternatives (SimpleCst).
refs : ( ra | rb ) C EOF ;

ra : A ;

rb : B ;

A : 'a' ;

B : 'b' ;

C : 'c' ;

WS : ' ' -> skip ;

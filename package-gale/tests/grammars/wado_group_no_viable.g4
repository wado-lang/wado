// A required group that takes none of its alternatives: ANTLR4 reports a
// no-viable-alternative there, one shape per group class.
grammar GroupNoViable;

options { language = Wado; }

// Token-only alternatives, one carrying an action.
tok : ( A { p.emit("a") } | B ) C EOF ;

// An alt-initial predicate that never holds.
gated : ( { false }? A { p.emit("x") } | B ) A EOF ;

// Rule-reference alternatives (SimpleCst).
refs : ( ra | rb ) C EOF ;

ra : A ;

rb : B ;

A : 'a' ;

B : 'b' ;

C : 'c' ;

WS : ' ' -> skip ;

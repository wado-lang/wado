// Source: distilled from SQLite.g4's `type_name : name+? ( '(' … ')' )?`.
// License: same as the Gale package.
//
// A non-greedy `+?` whose body FIRST equals the continuation's FIRST. The loop
// cannot be decided by lookahead alone: exiting is correct only when the rest
// of the input can still match, so `a a` takes one iteration and `a a a` takes
// two. Gale's static exit condition falls back to the minimum match (one
// iteration, never looping) and rejects the longer input — the divergence the
// driver test pins as `#[TODO]`.
grammar LlNonGreedyPlusLoop;

s : x+? y EOF ;
x : A ;
y : A ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;

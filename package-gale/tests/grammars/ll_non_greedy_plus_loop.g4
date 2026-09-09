// The shape of `type_name : name+? ( '(' … ')' )?` in the `SQLite.g4` beside
// this file: MIT, Copyright (c) 2014 by Bart Kiers,
// https://github.com/bkiers/sqlite-parser. The rules below are written here.
//
// A non-greedy `+?` whose body FIRST equals the continuation's FIRST. The loop
// cannot be decided by lookahead alone: exiting is correct only when the rest
// of the input can still match, so `a a` takes one iteration and `a a a` takes
// two. Gale takes the minimum match instead and rejects the longer input; the
// driver test pins that divergence as `#[TODO]`.
grammar LlNonGreedyPlusLoop;

s : x+? y EOF ;
x : A ;
y : A ;

A : 'a' ;
WS : [ \t\r\n]+ -> skip ;

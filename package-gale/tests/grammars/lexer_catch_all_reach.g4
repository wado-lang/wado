// Source: Gale test fixture (catch-all reachability from a dispatch branch)
// License: same as the Gale package
//
// The mode dispatch routes on the first character, and a catch-all (`C : .`)
// is left out of every specific branch on the assumption that a rule reaching
// that branch matches at least one char. That holds only for a rule whose
// body IS one char: `A` and `D` both have a suffix that can fail, so `e` and
// `h` reach their branch, fail there, and must still fall to `C`.
//
// `B` is the other half — its body is a single char, so it cannot fail once
// its branch is entered and the catch-all stays out of that branch.
//
// Char classes rather than literals throughout: a literal-only rule is hoisted
// into the keyword classifier and would not exercise the dispatch at all.
// `K` is the third way a branch falls through: its body is one char, but a
// predicate nested in the group can reject it, so "matches on its first
// character" has to be judged below the rule's own alternative.
lexer grammar LexerCatchAllReach;

options { language = Wado; }

A : [a-f] '1' ;
B : [+] ;
D : [h] [0-9] ;
K : ([k] { false }? | [l]) ;
C : . ;

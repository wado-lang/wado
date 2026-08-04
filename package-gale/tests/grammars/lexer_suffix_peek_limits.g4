// Source: Gale test fixture (suffix shapes the peek has to lower faithfully)
// License: same as the Gale package
//
// The arm / iteration scoring peeks the suffix by emitting it a second time
// (`lexer_alt_suffix_shapes.g4`), so the peek has to lower it exactly as the
// commit will — anything weaker reports "no match" for input the commit would
// have matched, and the rule matches nothing at all.
//
// `A` / `B`: a greedy repeat under the suffix needs its own lookahead-aware
// loop (`[a-b]+` can eat the `'b'` that follows it), which only the sequence
// lowering gives it.
// `C` / `D`: a non-greedy repeat under the suffix needs its min-match; peeked
// greedily, `.*?` swallows the terminator.
// `E`: `F` resolves its own `caseInsensitive`, so its first char set is `X`
// *and* `x`, which `[a-z]` overlaps.
// `G`: a complement behind a rule reference cannot be read case-folded —
// folding narrows it, and `~'a'` does overlap `'A'+`.
lexer grammar LexerSuffixPeekLimits;

A : 'a'+ [a-b]+ 'b' ;
B : ('m' | 'mn') [n-o]+ 'o' ;
C : 'c'+ .*? 'z' ;
D : ('-' | '--') .*? 'q' ;
E : [e-h]+ F ;
G : 'A'+ H ;

F options { caseInsensitive = true; } : 'X' ;
H : ~'a' ;

J : . ;

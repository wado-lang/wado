// The recursive non-greedy shape written as one rule, where the recursion is
// the entry rule's own.
//
// `lexer_recursive_ng_fragment.g4` reaches it one call out, through a fragment.
// Here the rule that owns the match holds both the recursion and the `.*?`. They
// are still separate alternatives, so the `"` the loop must stop at is a
// different one at each `#` depth, exactly as it is there. A walk that starts by
// marking the entry rule visited never asks whether it recurses into itself, and
// routes this to the static path where the min-match takes the first `"`.
lexer grammar LexerRecursiveNgSelf;

STR
    : '#' STR '#'
    | '"' .*? '"'
    ;

ID
    : [a-z]+
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

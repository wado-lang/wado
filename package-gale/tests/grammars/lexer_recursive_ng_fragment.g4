// Source: hand-written for Gale's lexer tests.
// License: same as the Gale package.
//
// A non-greedy loop whose stopping point the rule's own recursion decides,
// reached through a fragment. `STR_BODY` recurses in one alternative and holds
// the `.*?` in the other, so the `"` the loop must stop at is a different one
// at each `#` depth — the min-match takes the first `"` and strands the rest.
//
// This is `RustLexer.g4`'s
// `RAW_STRING_LITERAL : 'r' RAW_STRING_CONTENT` over
// `fragment RAW_STRING_CONTENT : '#' RAW_STRING_CONTENT '#' | '"' .*? '"'`,
// where it costs every `r#"..."#` holding a quote.
lexer grammar LexerRecursiveNgFragment;

STR
    : 'r' STR_BODY
    ;

fragment STR_BODY
    : '#' STR_BODY '#'
    | '"' .*? '"'
    ;

ID
    : [a-z]+
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

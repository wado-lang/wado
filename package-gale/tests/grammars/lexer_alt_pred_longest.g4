// An alternation in tail position whose arms reach different lengths, where
// one arm carries a semantic predicate. ANTLR4's lexer takes the longest
// match; the predicate decides whether its own arm matches at all, not how the
// lengths compare. Scoring only the predicate-free arms — or stopping at the
// first arm that matches — takes the short one.
//
// This is `RustLexer.g4`'s
// `FLOAT_LITERAL : {…}? (DEC_LITERAL '.' {…}? | DEC_LITERAL ('.' DEC_LITERAL)? …)`,
// where taking arm 0 lexed `0.75` as `0.` plus `75`.
lexer grammar LexerAltPredLongest;

options {
    language = Wado;
}

NUM
    : (
        DIGITS '.' { pos - start != 3 }?
        | DIGITS ('.' DIGITS)?
    )
    ;

fragment DIGITS
    : [0-9]+
    ;

DOT
    : '.'
    ;

ID
    : [a-z]+
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

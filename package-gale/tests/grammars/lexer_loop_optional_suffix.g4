// A greedy loop followed by an optional suffix whose first character the loop
// also takes: `255_u8` is one NUM, as ANTLR4's longest match over the whole
// rule makes it. Stopping at the loop's end strands `u8` as an ID.

grammar LexerLoopOptionalSuffix;

text : (NUM | ID)* EOF ;

NUM : [0-9] [0-9_]* SUFFIX? ;

fragment SUFFIX : '_' ('u8' | 'i64') ;

ID : [a-z] [a-z0-9]* ;

WS : ' ' -> skip ;

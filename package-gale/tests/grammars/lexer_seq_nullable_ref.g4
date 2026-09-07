// Source: hand-written for Gale's lexer ATN tests.
// License: same as the Gale package.
//
// `STR`'s loop arms overlap, so the rule routes to `latn_match` rather than a
// static choice. One arm calls the same nullable fragment twice in a row, which
// the ATN closure walks by entering `DASH`, reaching its stop through the `?`,
// and continuing in the caller — where the second call waits.
//
// The closure guards left recursion by refusing a rule it has already entered.
// That set has to shrink when a call returns, exactly as the return stack does,
// or the second `DASH` is refused as recursion and `"--a"` never lexes.
grammar LexerSeqNullableRef;

start
    : STR EOF
    ;

STR : '"' (DASH DASH 'a' | ESC)* '"' ;

fragment DASH : '-'? ;
fragment ESC  : '\\' . ;

WS : [ \t\r\n]+ -> skip ;

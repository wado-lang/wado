// Source: hand-written for Gale's lexer emit tests.
// License: same as the Gale package.
//
// A greedy loop whose body is an alternation with overlapping arms, where the
// longer arm is the reading: `STR : '"' (~["] | ESC)* '"'` over `"a\"b"`. `~["]`
// matches the backslash on its own, so a first-match loop ends the token at the
// escaped quote and leaves `b"` behind.
//
// `HAZARD` is the shape that must not move with it: `('a' | 'ab')* 'b'` over
// `ab`, where taking the longer arm strands the suffix and only the shorter one
// lets the rule match at all.
grammar LexerLoopArmLongest;

start
    : (STR | HAZARD | ID)+ EOF
    ;

STR
    : '"' (~["] | ESC)* '"'
    ;

fragment ESC
    : '\\' ['"]
    ;

HAZARD
    : ('a' | 'ab')* 'b'
    ;

ID
    : [c-z]+
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

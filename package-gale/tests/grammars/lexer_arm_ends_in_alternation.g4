// An arm of a group that a suffix follows ends in a fragment whose arms share
// a prefix and differ in length. The group's arms are scored against the `}`,
// but the fragment, matched first-match, settles on `1f6` in `{1f600}` before
// the `}` is seen: the choice has to reach the group's scoring.
grammar LexerArmEndsInAlternation;

start
    : ESC+ EOF
    ;

ESC
    : '{' ('0'+ | '0'* DIGITS) '}'
    ;

fragment DIGITS
    : [1-9a-f] H? H?
    | [1-9a-f] H H H H
    | '10' H H H H
    ;

fragment H
    : [0-9a-f]
    ;

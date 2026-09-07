// Source: hand-written for Gale's LL prediction tests.
// License: same as the Gale package.
//
// Three loops nested inside one another, the innermost behind a closing brace:
// `fields` iterates on a comma inside `{ … }`, `alt` on a bar, and `elems` on a
// comma again. Each hands the one below it an alternation — another iteration,
// or what it was itself called with — so the continuation reaching `fields` is
// `}` ahead of two of them.
//
// An alternation begins where it was written. Composing `}` in front moves that
// point without moving the mark, and a gate that revives the reading at the
// `}`'s own position sees the comma and hands `fields`' own separator to
// `elems`. One level of nesting does not show it: the revived reading dies on
// the token after the comma. Two do.
//
// `Wado.g4` spells this as a struct pattern inside a list pattern:
// `let [{ x, y }] = p`, where `pattern`'s `('|' patternRange)*` is the middle
// loop.
grammar LlNestedCommaLoops;

start
    : list EOF
    ;

list
    : LBRACK elems? RBRACK
    ;

elems
    : alt (COMMA alt)*
    ;

alt
    : item (PIPE item)*
    ;

item
    : LBRACE fields? RBRACE
    | ID
    ;

fields
    : ID (COMMA ID)*
    ;

ID     : [a-z]+ ;
COMMA  : ',' ;
PIPE   : '|' ;
LBRACE : '{' ;
RBRACE : '}' ;
LBRACK : '[' ;
RBRACK : ']' ;
WS     : [ \t\r\n]+ -> skip ;

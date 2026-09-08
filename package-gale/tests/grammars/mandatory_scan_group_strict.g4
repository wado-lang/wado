// A required scan group is emitted lenient by default: finding no alternative
// it rewinds to its entry position and the rule reports a match. That is an
// over-approximation the longest-match tournament wants -- except where the
// group is mandatory and every alternative is scannable, and then it is simply
// wrong: `attr* (decl | call)` over `# a x` matches the attribute, finds
// neither alternative, and reports the attribute's length as a `stmt`. The
// parse cannot reproduce it, and the tournament has already committed.
//
// This is `RustParser.g4`'s `item : outerAttribute* (visItem | macroItem)`,
// which is why `fn f() -> T { #[a] x }` does not parse: `#[a]` scans as an
// `item`, so `statement+ expression?` ties with the lone `expression` that is
// the real reading and wins the tie.
grammar MandatoryScanGroupStrict;

start
    : block EOF
    ;

block
    : LB body RB
    ;

body
    : stmt+ expr?
    | expr
    ;

stmt
    : attr* (decl | call)
    ;

decl
    : KW_LET ID SEMI
    ;

call
    : ID LP RP SEMI
    ;

expr
    : attr* ID
    ;

attr
    : POUND ID
    ;

KW_LET : 'let' ;
POUND  : '#' ;
SEMI   : ';' ;
LP     : '(' ;
RP     : ')' ;
LB     : '{' ;
RB     : '}' ;
ID     : [a-z]+ ;
WS     : [ \t\r\n]+ -> skip ;

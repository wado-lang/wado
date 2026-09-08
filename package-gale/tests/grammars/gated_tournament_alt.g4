// An alternative the lookahead cannot separate from its sibling, decided by
// length — and gated by a semantic predicate a caller toggles. `expr`'s first
// alternative is strictly longer, so the tournament always takes it; inside
// `stmt`'s `IF` head the counter is non-zero and it must not exist at all, so
// the shorter one wins and leaves the block to the caller.
//
// A scan is a pure `(tokens, pos, follow)` function and cannot ask a parser
// predicate, so the gate has to be evaluated where the parser is in scope and
// score the refused alternative as no match.
//
// This is `RustParser.g4`'s "no struct literal in a condition": `if a { b }` is
// `a` and a block, not the struct literal `a { b }`.
grammar GatedTournamentAlt;

@parser::members {
    int noBrace = 0;
}

start
    : stmt+ EOF
    ;

stmt
    : IF {noBrace = noBrace + 1;} expr {noBrace = noBrace - 1;} block
    | expr SEMI
    ;

block
    : LB ID RB
    ;

// Left-recursive on purpose: an LR rule's atom alternatives are chosen by the
// `_sd_` scan tournament, which is the dispatch that has to evaluate the gate.
// A plain two-alt rule takes the predicate-gated chain instead and would pass
// either way.
expr
    : expr PLUS expr
    | {noBrace == 0}? ID LB ID RB
    | ID
    ;

IF   : 'if' ;
PLUS : '+' ;
ID   : [a-z]+ ;
LB   : '{' ;
RB   : '}' ;
SEMI : ';' ;
WS   : [ \t\r\n]+ -> skip ;

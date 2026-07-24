// Regression for issue #1671: a mandatory (non-nullable) leading alternation
// group whose FIRST set is reachable through the rule's own recursive tail.
// The scan predicate for such a rule must fail when the group does not match,
// not bail out lenient (matching zero tokens) and fall into the recursive
// tail — that recurses without consuming input and traps (stack exhausted).
//
// Mirrors Wado's `closure : ('||' | '|' closureParamList? '|') expression`,
// where `|` is in FIRST(closure) ⊆ FIRST(expr) via `primary : closure | ...`.
grammar ScanGroupRecursion;

// Two alts share the whole `expr` prefix, so choosing between them needs a
// lookahead scan of `expr` — the entry point of the scan cycle.
prog    : item EOF ;
item    : expr ';'
        | expr '!'
        ;
expr    : primary ;
primary : closure | INT ;
closure : ('||' | '|' '|') expr ;

INT      : [0-9]+ ;
PIPEPIPE : '||' ;
PIPE     : '|' ;
BANG     : '!' ;
SEMI     : ';' ;
WS       : [ \t\r\n]+ -> skip ;

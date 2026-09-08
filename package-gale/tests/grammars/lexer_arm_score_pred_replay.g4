// An arm-scoring group whose arms carry both a semantic predicate and actions.
//
// The rule's actions put it on the replay path, and a replay body runs with
// predicates off: re-evaluating a state-dependent one could reject the winner's
// actions after the token has committed. A selection pass is the opposite. It
// makes the decision, so it runs the predicates the match ran. Give it the
// body's setting and it scores an arm the match had disabled, and the replay
// runs the actions of an arm the token was never built from.
//
// `T` separates the two: with the predicate arm 0 wins and the token is `ab`;
// without it arm 1 reaches one further and wins the replay alone. `MARK` reads
// which arm's action ran.
lexer grammar LexerArmScorePredReplay;

options {
    language = Wado;
}

@members {
    hits: i32 = 0
}

// `pos == start` at the group, so this predicate is false and arm 1 is out.
T
    : ( 'a' { lx.hits = 1 } | { pos - start != 0 }? 'ab' { lx.hits = 10 } ) [b-z]
    ;

MARK
    : { lx.hits == 1 }? '!' { lx.hits = 0 }
    ;

BANG
    : '!'
    ;

C
    : 'c'
    ;

WS
    : [ \t\r\n]+ -> skip
    ;

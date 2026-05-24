// Regression: user lexer rule `ERROR` claims the same `TK_ERROR`
// identifier Gale uses for its no-match sentinel. `gen_token_constants`
// must let the user rule own it and skip the sentinel emit; the
// generated parser still compiles.

grammar TkErrorSentinelCollision;

prog : ID EOF;

ID    : [a-z]+;
ERROR : .;

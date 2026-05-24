// Regression min repro: user-defined lexer rule `ERROR` lines up on the
// same `TK_ERROR` identifier Gale uses for its internal no-match
// sentinel. Pre-fix, `gen_token_constants` emitted the global twice
// and `wado compile` rejected with `duplicate definition 'TK_ERROR'`.
//
// Post-fix (lexer_gen.wado:gen_token_constants /
// gen_token_kind_name_fn), Gale lets the user rule own the
// `TK_ERROR` identifier and skips the sentinel emit — the canonical
// `ERROR : .;` shape is itself a catch-all aligned with the
// no-match role, so collapsing onto one kind preserves behaviour.
// This file pins that collapse: any future regression would surface
// here first.
//
// Originally surfaced via ANTLR4 runtime-testsuite descriptors
// Performance/ExpressionGrammar_{1,2} (their old `[stage_a_skip]`
// blamed an "all-binary-op LR rewrite bug" — the rewrite path is
// in fact clean, the sentinel collision was the only blocker).

grammar TkErrorSentinelCollision;

prog : ID EOF;

ID    : [a-z]+;
ERROR : .;

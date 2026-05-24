// Min repro: user-defined lexer rule `ERROR` collides with Gale's
// internal `TK_ERROR` sentinel global, producing two
// `global TK_ERROR: i32 = ...;` lines in the generated Wado.
//
// Today: `wado compile` rejects the generated Wado with
//   error: duplicate definition 'TK_ERROR'
//
// Covers stage_a_skip entries (status.toml triage reason is stale —
// the actual bug is the sentinel-name collision, not the all-binary-op
// LR rewrite path; that path emits clean code once `ERROR` is renamed
// to anything else):
//   - Performance/ExpressionGrammar_1
//   - Performance/ExpressionGrammar_2
//
// Fix direction: reserve / prefix Gale's internal sentinel token
// kinds (`TK_ERROR`, `TK_EOF`) so they cannot collide with any
// user-named token.

grammar TkErrorSentinelCollision;

prog : ID EOF;

ID    : [a-z]+;
ERROR : .;

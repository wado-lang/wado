// Regression min repro: parser rule references an uppercase identifier
// (`K_IN`) that has no defining lexer rule. ANTLR4 silently treats
// such a name as an implicit token type (see
// vendor/antlr4/doc/lexer-rules.md); pre-fix, Gale's post-merge
// `check_references` rejected the grammar with
// `reference to undefined token "K_IN" in "expr"`.
//
// Post-fix (`ir.wado::synthesize_implicit_tokens`, wired into the
// CLI / kiln / parse_checked paths before `check_references`), Gale
// pushes a `LexerRule::virtual_token` for every such reference; the
// lexer cannot produce the token on its own but downstream codegen
// gets a `TK_<name>` global it can reference. This file pins that
// behaviour: any future regression would surface here first.
//
// Originally surfaced via ANTLR4 runtime-testsuite descriptors
// LeftRecursion/WhitespaceInfluence_{1,2}, which reference
// `K_IN / K_AND / K_OR / K_TRUE / K_FALSE / K_NULL` from a parser
// rule without defining them. Their old `[stage_a_skip]` reason
// blamed an "all-binary-op LR rewrite bug" — the rewrite path is
// in fact clean, the unresolved-token rejection was the only
// blocker.

grammar ParserRefImplicitToken;

prog : expr EOF;
expr : ID K_IN ID;

ID : [a-z]+;
WS : [ \t\n\r]+ -> skip;

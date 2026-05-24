// Regression: parser rule references an uppercase name (`K_IN`) that
// no lexer rule defines. ANTLR4 silently synthesises an implicit
// token type for it (vendor/antlr4/doc/lexer-rules.md);
// `synthesize_implicit_tokens` must do the same so `check_references`
// and codegen accept the grammar.

grammar ParserRefImplicitToken;

prog : expr EOF;
expr : ID K_IN ID;

ID : [a-z]+;
WS : [ \t\n\r]+ -> skip;

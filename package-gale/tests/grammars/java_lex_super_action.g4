// Source: Gale test fixture (Stage C superClass action ops, language = Java)
// License: same as the Gale package
//
// The real-world shape: a default (`language = Java`) lexer grammar whose
// actions are nothing but `{this.m();}` base calls — TypeScriptLexer's
// `ProcessOpenBrace`, ANTLRv4Lexer's `handleBeginArgument`. java2wado is carved
// out for a superClass grammar, so these bodies run only because the base-call
// rewrite turns each into Wado on its own. `NAME` keeps a predicate op in the
// same interface, so the base mixes Java-side roles as the real grammars do.
lexer grammar JavaLexSuperAction;

options { superClass = JsBase; }

OPEN  : '{' {this.processOpen();} ;
CLOSE : '}' {this.processClose();} ;
NAME  : {this.namesEnabled()}? [a-z]+ ;
ID    : [a-z]+ ;
WS    : [ \t\r\n]+ -> skip ;

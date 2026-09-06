// Source: hand-written for Gale's tutorial — the CSS half of a composite
// page grammar. Composed into `MiniHtml.g4` by its `import`.
// License: same as the Gale package.
//
// Two conventions make a grammar embeddable, and neither is a Gale feature —
// they fall out of what composition already is:
//
//   * The lexer rules live in `mode CSS`, so they are unreachable until the
//     host pushes into it. A composite has one lexer; without a mode, `TEXT`
//     over in `MiniHtml.g4` would swallow a stylesheet whole.
//   * The token names are prefixed. A composite has one token space, so the
//     first rule of a given name wins and the rest are dropped — `CSS_IDENT`
//     and `JS_IDENT` have to be different names to both survive.
//
// This grammar names no host. `MiniHtml.g4` declares `mode CSS` itself for the
// `</style>` that leaves it, and composition unifies the two declarations by
// name.
//
// Every name here is the same `CSS_IDENT` token. What each one *is* — a
// selector, a property, or a value — is where the parser put it, not anything
// the lexer saw:
//
//     color { color: color; }
//
// is a selector, a property and a value spelled identically.
// `MiniCss.highlights.scm` reads the three apart off the rule stack.
grammar MiniCss;

// No `EOF`: a stylesheet is a fragment of the host document, not a file.
stylesheet  : ruleset* ;
ruleset     : selector CSS_LBRACE declaration* CSS_RBRACE ;
selector    : CSS_IDENT (CSS_COMMA CSS_IDENT)* ;
declaration : property CSS_COLON value CSS_SEMI ;
property    : CSS_IDENT ;
value       : CSS_IDENT | CSS_NUMBER | CSS_HASH ;

mode CSS;
CSS_LBRACE  : '{' ;
CSS_RBRACE  : '}' ;
CSS_COLON   : ':' ;
CSS_SEMI    : ';' ;
CSS_COMMA   : ',' ;
CSS_HASH    : '#' [0-9a-fA-F]+ ;
CSS_NUMBER  : [0-9]+ ('px' | '%')? ;
CSS_IDENT   : [a-zA-Z-] [a-zA-Z0-9-]* ;
CSS_COMMENT : '/*' .*? '*/' -> channel(HIDDEN) ;
CSS_WS      : [ \t\r\n]+ -> skip ;

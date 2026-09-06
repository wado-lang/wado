// Source: hand-written for Gale's tutorial — the CSS half of a composite
// page grammar. Composed into `MiniHtml.g4` by its `import`.
// License: same as the Gale package.
//
// The mode and the `CSS_` prefixes are what make a grammar embeddable; both
// are consequences of composition rather than features, and `import.md`
// explains them. This grammar names no host.
//
// Every name here is the same `CSS_IDENT` token. What each one is — a selector,
// a property, or a value — is where the parser put it, not anything the lexer
// saw. In
//
//     color { color: color; }
//
// all three are spelled identically, and `MiniCss.highlights.scm` reads them
// apart off the rule stack.
// Deliberately left out, so a reader does not read a subset boundary as a
// limitation: class / id / descendant selectors, multi-token values, at-rules.
// What this grammar does accept, it accepts in every form CSS allows.
grammar MiniCss;

// No `EOF`: a stylesheet is a fragment of the host document, not a file.
stylesheet  : ruleset* ;
ruleset     : selector CSS_LBRACE declarations? CSS_RBRACE ;
selector    : CSS_IDENT (CSS_COMMA CSS_IDENT)* ;
// `;` separates declarations and is optional after the last, per the CSS
// grammar: `a { b: c }` is as valid as `a { b: c; }`.
declarations : declaration (CSS_SEMI declaration)* CSS_SEMI? ;
declaration : property CSS_COLON value ;
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

// Source: Hand-authored for Gale's recursive-fragment coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// ANTLR4 lexer fragments may reference themselves or each other,
// enabling lexer-time recursion. Canonical use case: matching a
// balanced brace block as a single token — the lexer has to recurse
// to handle arbitrary nesting depth. This is described in
// `vendor/antlr4/doc/lexer-rules.md` (Fragments).
//
// Gale's `fragment_is_recursive` detection in `lexer_gen.wado`
// promotes recursive fragments from inlined patterns to full match
// functions; this grammar exercises that code path end-to-end.

grammar recursive_lexer;

file : (BLOCK | IDENT)+ EOF ;

// The BLOCK token is everything from the opening `{` to the matching
// `}`, including nested blocks. The recursive fragment BRACED handles
// the nesting: inside the braces we can recurse into BRACED (for a
// nested block) or consume any non-brace character.
BLOCK : BRACED ;

fragment BRACED : '{' (BRACED | ~[{}])* '}' ;

IDENT : [a-zA-Z_][a-zA-Z_0-9]* ;

WS : [ \t\r\n]+ -> skip ;

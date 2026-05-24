// Regression fixture for the token-only group kind-check bug.
//
// A token-only set element `('b' | 'c')` used to be emitted as
// `let lit_b_or_lit_c = p.advance();` with no `peek_kind` guard, so
// any token at that position was silently consumed and the parse
// returned `Ok` even when the input violated the grammar. Surfaced
// by the ANTLR4 `ParserErrors/SingleTokenDeletionExpectingSet` and
// `ParserErrors/SingleTokenDeletionBeforeAlt` descriptors.
//
// After the fix the token-only group emit gates `p.advance()` on a
// kind-check against the union of its alternatives, so feeding a
// token outside the set returns `Err`.
grammar ParserSetGroupDispatch;

prog : 'a' ('b' | 'c') 'd' EOF ;

WS : ' ' -> skip ;

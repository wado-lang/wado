// Regression: an alternative that ends in two consecutive `EOF` references.
//
// Source: derived from ANTLR4 runtime-testsuite descriptor
//   ParserExec/MultipleEOFHandling.txt
//
// License: BSD 3-Clause (vendor/antlr4/LICENSE.txt) — derived test grammar.
//
// ANTLR4 lets a parser rule reference `EOF` more than once. The token
// stream still contains exactly one EOF sentinel; the second reference
// matches the same sentinel without advancing past it. Gale must not
// trap on the second `expect(TK_EOF)` when its caller is already
// positioned on EOF.

grammar MultipleEof;

prog : ('x' | 'x' 'y') EOF EOF ;

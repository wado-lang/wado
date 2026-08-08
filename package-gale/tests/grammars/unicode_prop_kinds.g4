// Source: Hand-authored for Gale's Unicode property coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// The property surface beyond the general categories: a script, a block
// (spelled with the `In` prefix ANTLR4 documents), and a binary property.
// The pair `GREEK` / `INGREEK` is the point — a script spans blocks, so
// `ᾰ` (U+1FB0, Greek Extended) is Script=Greek but not in the
// `Greek_And_Coptic` block. Token kinds oracle-checked against the
// published jar.
lexer grammar unicode_prop_kinds;

GREEK   : [\p{Greek}]+ ;
INGREEK : [\p{InGreek}]+ ;
PATSYN  : [\p{Pattern_Syntax}]+ ;
WSP     : [\p{White_Space}]+ -> skip ;
OTHER   : . ;

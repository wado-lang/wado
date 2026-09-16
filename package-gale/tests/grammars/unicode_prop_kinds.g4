// The property surface beyond the general categories: a script, a category, a
// binary property, and an enumerated `Property=Value` name. The pair `GREEK` /
// `GREEKMARK` is the point — a script and a category cut across each other, so
// `ᾰ` (U+1FB0) is Script=Greek and `Ll` while U+0300, which composes with it,
// is `Mn` and Script=Inherited. Token kinds oracle-checked against the
// published jar.
//
// The Block property is absent because Gale does not answer it; see
// `antlr4-compatibility.md`.
lexer grammar unicode_prop_kinds;

GREEK     : [\p{Greek}]+ ;
GREEKMARK : [\p{Mn}]+ ;
PATSYN    : [\p{Pattern_Syntax}]+ ;
WIDE      : [\p{ea=W}]+ ;
WSP       : [\p{White_Space}]+ -> skip ;
OTHER     : . ;

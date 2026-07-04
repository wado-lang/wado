// Source: Gale test fixture (Stage C parser context API)
// License: same as the Gale package
//
// `$text` is the input the current rule has consumed so far, emitted from a
// captured rule-start byte through `p.text_span(...)`. `p.la(k)` reads the
// lookahead token kind k ahead (a raw Wado predicate body passes through the
// identity translator verbatim).
grammar WadoRuleText;

options { language = Wado; }

r : ID ID { p.emit($text); } ;

ID : [a-z]+ ;
WS : ' ' -> skip ;

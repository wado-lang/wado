// Source: Gale test fixture (Stage C token member access)
// License: same as the Gale package
//
// `$x.text` reads the matched text of a labeled token through the context
// API (`p.token_text(x)`), where `x` is the token's stream index bound by the
// call site (`let x = p.expect(...)`).
grammar WadoTokenText;

options { language = Wado; }

r : a=ID '+' b=ID { p.emit(`{$a.text}-{$b.text}`); } ;

ID : [a-z]+ ;
WS : ' ' -> skip ;

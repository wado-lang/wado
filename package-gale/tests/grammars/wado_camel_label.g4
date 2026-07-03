// Source: Gale test fixture (Stage C camelCase binding)
// License: same as the Gale package
grammar WadoCamelLabel;
options { language = Wado; }
r : myTok=ID { p.emit($myTok.text); } ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

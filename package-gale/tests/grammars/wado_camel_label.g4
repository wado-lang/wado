// Stage C camelCase binding.
grammar WadoCamelLabel;
options { language = Wado; }
r : myTok=ID { p.emit($myTok.text); } ;
ID : [a-z]+ ;
WS : ' ' -> skip ;

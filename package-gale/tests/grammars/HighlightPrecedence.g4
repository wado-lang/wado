grammar HighlightPrecedence;

s : item* EOF ;
item : 'tag' tagName | 'id' identifier | 'fld' fieldName ;
tagName : identifier ;
fieldName : identifier ;
identifier : ID | 'from' ;

ID : [a-z]+ ;
COMMENT : '/*' .*? '*/' -> channel(HIDDEN) ;
WS : [ \t\r\n]+ -> skip ;

grammar T;
s @after {<ToStringTree("$ctx"):writeln()>} : expr EOF ;
expr : literal
     | op expr
     | expr op expr
     ;
literal : '-'? Integer ;
op : '+' | '-' ;
Integer : [0-9]+ ;
WS : (' '|'\n') -> skip ;

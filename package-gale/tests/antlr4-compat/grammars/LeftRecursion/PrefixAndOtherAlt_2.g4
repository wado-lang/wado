grammar T;
s @after {System.out.println($ctx.toStringTree(this));} : expr EOF ;
expr : literal
     | op expr
     | expr op expr
     ;
literal : '-'? Integer ;
op : '+' | '-' ;
Integer : [0-9]+ ;
WS : (' '|'\n') -> skip ;

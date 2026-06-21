grammar Between;
prog : expr EOF ;
expr : INT                          # Num
     | expr 'and' expr              # And
     | expr 'or' expr               # Or
     | 'between' expr 'and' expr    # Between
     ;
INT : [0-9]+ ;
WS  : [ \t\r\n]+ -> skip ;

grammar T;
prog
@init {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);}
   : expr expr {System.out.println("alt 1");}
   | expr
   ;
expr: '@'
   | ID '@'
   | ID
   ;
ID  : [a-z]+ ;
WS  : [ \r\n\t]+ -> skip ;

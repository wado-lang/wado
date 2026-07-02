grammar T;
s
@init {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);}
@after {dumpDFA();}
   : '{' stat* '}' ;
stat: 'if' ID 'then' stat ('else' ID)?
       | 'return'
       ;
ID : 'a'..'z'+ ;
WS : (' '|'\t'|'\n')+ -> skip ;

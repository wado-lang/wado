grammar T;
s
@init {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);}
:   expr[0] {System.out.println($expr.ctx.toStringTree(this));};
   expr[int _p]
       : ID
       (
  {5 >= $_p}? '*' expr[6]
  | {4 >= $_p}? '+' expr[5]
       )*
       ;
ID  : [a-zA-Z]+ ;
WS  : [ \r\n\t]+ -> skip ;

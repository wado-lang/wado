grammar T;
prog
@init {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);}
   : expr_or_assign*;
expr_or_assign
   : expr '++' {System.out.println("fail.");}
   |  expr {System.out.println("pass: " + $expr.text);}
   ;
expr: expr_primary ('<-' ID)?;
expr_primary
   : '(' ID ')'
   | ID '(' ID ')'
   | ID
   ;
ID  : [a-z]+ ;

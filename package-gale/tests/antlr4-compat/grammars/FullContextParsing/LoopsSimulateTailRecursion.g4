grammar T;
prog
@init {<LL_EXACT_AMBIG_DETECTION()>}
   : expr_or_assign*;
expr_or_assign
   : expr '++' {<writeln("\"fail.\"")>}
   |  expr {<AppendStr("\"pass: \"","$expr.text"):writeln()>}
   ;
expr: expr_primary ('<-' ID)?;
expr_primary
   : '(' ID ')'
   | ID '(' ID ')'
   | ID
   ;
ID  : [a-z]+ ;

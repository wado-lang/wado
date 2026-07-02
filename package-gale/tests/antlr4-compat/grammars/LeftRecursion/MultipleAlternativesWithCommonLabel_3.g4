grammar T;
s : e {System.out.println($e.v);};
e returns [int v]
  : e '*' e     {$v = ((BinaryContext)$ctx).e(0).v * ((BinaryContext)$ctx).e(1).v;}  # binary
  | e '+' e     {$v = ((BinaryContext)$ctx).e(0).v + ((BinaryContext)$ctx).e(1).v;}  # binary
  | INT{$v = $INT.int;} # anInt
  | '(' e ')'   {$v = $e.v;}     # parens
  | left=e INC  {assert ((UnaryContext)$ctx).INC() != null;$v = $left.v + 1;}      # unary
  | left=e DEC  {assert ((UnaryContext)$ctx).DEC() != null;$v = $left.v - 1;}      # unary
  | ID {$v = 3;}        # anID
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+ ;
INC : '++' ;
DEC : '--' ;
WS : (' '|'\n') -> skip;

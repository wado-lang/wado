grammar T;
s : e {System.out.println($e.v);};
e returns [int v, List<String> ignored]
  : a=e '*' b=e {$v = $a.v * $b.v;}
  | a=e '+' b=e {$v = $a.v + $b.v;}
  | INT {$v = $INT.int;}
  | '(' x=e ')' {$v = $x.v;}
  ;
INT : '0'..'9'+ ;
WS : (' '|'\n') -> skip ;

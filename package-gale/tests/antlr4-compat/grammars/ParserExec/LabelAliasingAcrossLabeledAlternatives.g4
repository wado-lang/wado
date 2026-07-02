grammar T;
start : a* EOF;
a
  : label=subrule {System.out.println($label.text);} #One
  | label='y' {System.out.println($label.text);} #Two
  ;
subrule : 'x';
WS : (' '|'\n') -> skip ;

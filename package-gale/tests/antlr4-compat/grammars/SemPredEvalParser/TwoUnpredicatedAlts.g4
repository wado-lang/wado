grammar T;
s : {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);} a ';' a; // do 2x: once in ATN, next in DFA
a : ID {System.out.println("alt 1");}
  | ID {System.out.println("alt 2");}
  | {false}? ID {System.out.println("alt 3");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;

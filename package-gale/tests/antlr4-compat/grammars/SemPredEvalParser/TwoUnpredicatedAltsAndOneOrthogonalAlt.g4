grammar T;
s : {getInterpreter().setPredictionMode(PredictionMode.LL_EXACT_AMBIG_DETECTION);} a ';' a ';' a;
a : INT {System.out.println("alt 1");}
  | ID {System.out.println("alt 2");} // must pick this one for ID since pred is false
  | ID {System.out.println("alt 3");}
  | {false}? ID {System.out.println("alt 4");}
  ;
ID : 'a'..'z'+ ;
INT : '0'..'9'+;
WS : (' '|'\n') -> skip ;

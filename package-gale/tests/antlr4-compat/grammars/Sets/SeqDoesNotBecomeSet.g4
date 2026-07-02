grammar T;
a : C {System.out.println(_input.getText());} ;
fragment A : '1' | '2';
fragment B : '3' '4';
C : A | B;

grammar T;
a : A {System.out.println($A.text);} ;
A : ~'b' ;

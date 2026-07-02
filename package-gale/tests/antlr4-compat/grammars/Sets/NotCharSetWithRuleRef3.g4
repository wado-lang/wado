grammar T;
a : A {System.out.println($A.text);} ;
A : ('a'|B) ;  // this doesn't collapse to set but works
fragment
B : ~('a'|'c') ;

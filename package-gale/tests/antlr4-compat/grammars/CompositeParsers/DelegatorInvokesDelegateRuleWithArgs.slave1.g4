parser grammar S;
a[int x] returns [int y] : B {System.out.print("S.a");} {$y=1000;} ;

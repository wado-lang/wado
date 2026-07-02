grammar T;
a : 'a' x=('b'|'c') {System.out.println("conjured=" + $x);} 'd' ;

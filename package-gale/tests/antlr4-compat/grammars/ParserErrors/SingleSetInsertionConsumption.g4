grammar T;
myset: ('b'|'c') ;
a: 'a' myset 'd' {System.out.println("" + $myset.stop);} ; 

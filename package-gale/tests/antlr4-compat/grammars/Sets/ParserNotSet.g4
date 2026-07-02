grammar T;
a : t=~('x'|'y') 'z' {System.out.println($t.text);} ;

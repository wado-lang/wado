grammar T;

program : state*{System.out.println($text);} EOF ;
state: 'break;' | 'continue;' | 'return;' ;

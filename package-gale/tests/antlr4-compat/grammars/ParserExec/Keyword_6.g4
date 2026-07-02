grammar T;

program : reset*{System.out.println($text);} EOF ;
reset: 'break;' | 'continue;' | 'return;' ;

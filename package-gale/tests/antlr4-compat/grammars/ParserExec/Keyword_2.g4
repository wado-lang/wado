grammar T;

program : sempred*{System.out.println($text);} EOF ;
sempred: 'break;' | 'continue;' | 'return;' ;

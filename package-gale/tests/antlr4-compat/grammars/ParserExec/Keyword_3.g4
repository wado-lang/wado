grammar T;

program : action*{System.out.println($text);} EOF ;
action: 'break;' | 'continue;' | 'return;' ;

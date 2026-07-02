grammar T;

program : addErrorListener*{System.out.println($text);} EOF ;
addErrorListener: 'break;' | 'continue;' | 'return;' ;

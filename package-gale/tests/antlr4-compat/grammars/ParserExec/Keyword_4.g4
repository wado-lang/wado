grammar T;

program : ruleIndexMap*{System.out.println($text);} EOF ;
ruleIndexMap: 'break;' | 'continue;' | 'return;' ;

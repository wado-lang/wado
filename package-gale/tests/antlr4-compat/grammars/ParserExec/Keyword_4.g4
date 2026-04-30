grammar T;

program : ruleIndexMap*{<writeln("$text")>} EOF ;
ruleIndexMap: 'break;' | 'continue;' | 'return;' ;

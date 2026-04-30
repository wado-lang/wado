grammar T;

program : addErrorListener*{<writeln("$text")>} EOF ;
addErrorListener: 'break;' | 'continue;' | 'return;' ;

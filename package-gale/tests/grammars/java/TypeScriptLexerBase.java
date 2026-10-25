// The Java twin of `impl TypeScriptLexerBase for TsBase` in
// package-gale/tools/typescript_recognizer.wado, so `antlr4_oracle.wado --super`
// runs ANTLR4 against the base class Gale runs against. Edit both or neither.
import java.util.ArrayList;
import org.antlr.v4.runtime.CharStream;
import org.antlr.v4.runtime.Lexer;
import org.antlr.v4.runtime.Token;

public abstract class TypeScriptLexerBase extends Lexer {
    // One entry per `${` interpolation still open: how many plain `{` are open
    // inside it. A `}` ends the interpolation only at count 0.
    private final ArrayList<Integer> interpBraces = new ArrayList<>();
    // Where each open template's interpolation entries start, so a closing
    // backtick drops what its interpolations left behind.
    private final ArrayList<Integer> templateMarks = new ArrayList<>();
    private boolean strict = false;
    private int lastToken = -1;

    public TypeScriptLexerBase(CharStream input) {
        super(input);
    }

    @Override
    public Token emit() {
        Token t = super.emit();
        if (t.getChannel() == Token.DEFAULT_CHANNEL) {
            lastToken = t.getType();
        }
        return t;
    }

    // Whether a token ends an operand, so a following `/` is a division rather
    // than the start of a regular expression.
    private static boolean endsOperand(int kind) {
        switch (kind) {
            case TypeScriptLexer.Identifier:
            case TypeScriptLexer.NullLiteral:
            case TypeScriptLexer.BooleanLiteral:
            case TypeScriptLexer.This:
            case TypeScriptLexer.CloseBracket:
            case TypeScriptLexer.CloseParen:
            case TypeScriptLexer.OctalIntegerLiteral:
            case TypeScriptLexer.DecimalLiteral:
            case TypeScriptLexer.HexIntegerLiteral:
            case TypeScriptLexer.StringLiteral:
            case TypeScriptLexer.PlusPlus:
            case TypeScriptLexer.MinusMinus:
                return true;
            default:
                return false;
        }
    }

    protected boolean IsRegexPossible() {
        return !endsOperand(lastToken);
    }

    protected boolean IsInTemplateString() {
        if (templateMarks.isEmpty() || interpBraces.isEmpty()) {
            return false;
        }
        return interpBraces.get(interpBraces.size() - 1) == 0;
    }

    protected boolean IsStrictMode() {
        return strict;
    }

    protected void ProcessOpenBrace() {
        int n = interpBraces.size();
        if (n > 0) {
            interpBraces.set(n - 1, interpBraces.get(n - 1) + 1);
        }
    }

    protected void ProcessCloseBrace() {
        int n = interpBraces.size();
        if (n > 0 && interpBraces.get(n - 1) > 0) {
            interpBraces.set(n - 1, interpBraces.get(n - 1) - 1);
        }
    }

    protected void ProcessStringLiteral() {
        String text = getText();
        if (text.equals("'use strict'") || text.equals("\"use strict\"")) {
            strict = true;
        }
    }

    protected void IncreaseTemplateDepth() {
        templateMarks.add(interpBraces.size());
    }

    protected void DecreaseTemplateDepth() {
        if (!templateMarks.isEmpty()) {
            int mark = templateMarks.remove(templateMarks.size() - 1);
            while (interpBraces.size() > mark) {
                interpBraces.remove(interpBraces.size() - 1);
            }
        }
    }

    protected void StartTemplateString() {
        interpBraces.add(0);
    }
}

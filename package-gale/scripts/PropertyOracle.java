// Driver for antlr4-property-oracle.sh: feed every Unicode scalar to the
// generated PropOracle lexer and print one `RULE<TAB>START-END` line (hex) per
// maximal run. A span no rule claims prints as `?`.
import org.antlr.v4.runtime.CharStreams;
import org.antlr.v4.runtime.CommonTokenStream;
import org.antlr.v4.runtime.Lexer;
import org.antlr.v4.runtime.Token;

public class PropertyOracle {
    public static void main(String[] args) throws Exception {
        // Surrogates cannot appear in the input, so stream index i is
        // points[i], not i itself.
        int[] points = new int[0x110000];
        StringBuilder text = new StringBuilder();
        int count = 0;
        for (int c = 0; c <= 0x10FFFF; c++) {
            if (c >= 0xD800 && c <= 0xDFFF) continue;
            text.appendCodePoint(c);
            points[count++] = c;
        }

        Lexer lexer = new PropOracle(CharStreams.fromString(text.toString()));
        lexer.removeErrorListeners();
        CommonTokenStream stream = new CommonTokenStream(lexer);
        stream.fill();

        StringBuilder out = new StringBuilder();
        int next = 0;
        for (Token t : stream.getTokens()) {
            if (t.getType() == Token.EOF) continue;
            if (t.getStartIndex() > next) {
                emit(out, "?", points[next], points[t.getStartIndex() - 1]);
            }
            emit(out, lexer.getVocabulary().getSymbolicName(t.getType()),
                 points[t.getStartIndex()], points[t.getStopIndex()]);
            next = t.getStopIndex() + 1;
        }
        if (next < count) {
            emit(out, "?", points[next], points[count - 1]);
        }
        System.out.print(out);
    }

    private static void emit(StringBuilder out, String rule, int start, int end) {
        out.append(rule).append('\t')
           .append(String.format("%04X", start)).append('-')
           .append(String.format("%04X", end)).append('\n');
    }
}

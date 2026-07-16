// ANTLR4 Java SQLite parser benchmark: reference for the Gale-generated parser.
//
// Parses the same queries.sql the Wado (Gale) row does, using a parser ANTLR4
// generated from the same SQLite.g4. This is the head-to-head for "Gale's
// generated parser vs ANTLR4's own", on identical grammar and input — on the
// JVM (JIT-warmed) rather than Wasm/native.
//
// SQLiteLexer/SQLiteParser are generated at build time from SQLite.g4 (see
// antlr4_java_bench.mjs); they are not vendored. Reports parse throughput
// (MB/s) with the iteration count auto-calibrated to run for about a second,
// matching the other benchmark harnesses.
//
// Prediction: the default (full-context LL). The two-stage SLL fast path is not
// used because SLL cannot resolve this grammar's ambiguities on this input (it
// bails and would always fall back to LL), so LL-only is ANTLR4's best-case
// configuration here.

import org.antlr.v4.runtime.CharStreams;
import org.antlr.v4.runtime.CommonTokenStream;
import java.nio.file.Files;
import java.nio.file.Paths;

public class Antlr4SqliteBench {
    static void parseOnce(String sql) {
        SQLiteLexer lexer = new SQLiteLexer(CharStreams.fromString(sql));
        lexer.removeErrorListeners();
        SQLiteParser parser = new SQLiteParser(new CommonTokenStream(lexer));
        parser.removeErrorListeners();
        parser.parse();
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 1) {
            System.err.println("usage: Antlr4SqliteBench <queries.sql>");
            System.exit(1);
        }
        String sql = new String(Files.readAllBytes(Paths.get(args[0])), "UTF-8");
        int size = sql.getBytes("UTF-8").length;

        // Warm the JIT (C2) and ANTLR's static DFA cache to steady state before
        // measuring — the most Java-favourable condition. On this grammar the
        // per-iteration time flattens by ~40-50 parses (the cold first parse is
        // ~3x the steady cost); warm past that knee on any machine by requiring
        // both a time and an iteration floor.
        long warmUntil = System.nanoTime() + 8_000_000_000L;
        long warmIters = 0;
        while (System.nanoTime() < warmUntil || warmIters < 60) {
            parseOnce(sql);
            warmIters++;
        }

        final long target = 1_000_000_000L; // ~1s measured window
        long n = 1, elapsed = 0;
        while (true) {
            long start = System.nanoTime();
            for (long i = 0; i < n; i++) {
                parseOnce(sql);
            }
            elapsed = System.nanoTime() - start;
            if (elapsed >= target) {
                break;
            }
            long next = n * target / Math.max(elapsed, 1);
            if (next <= n) {
                break;
            }
            n = Math.min(next, n * 100);
        }

        double rate = (double) size * n / (elapsed / 1e9);
        double msPerIter = elapsed / 1e6 / n;
        System.out.printf("antlr4-java (parse): %.2f MB/s   (%.3f ms/iter, %d iter)%n",
            rate / 1e6, msPerIter, n);
    }
}

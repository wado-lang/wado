#!/usr/bin/env python3
"""
Binary search (ternary search) for optimal inlining thresholds.
Minimizes zlib + fts combined elapsed time.
"""
import re
import subprocess
import sys

OPTIMIZE_RS = "wado-compiler/src/optimize.rs"
WADO_BIN = "./target/release/wado"
ZLIB_BENCH = "benchmark/zlib/zlib_bench.wado"
FTS_BENCH = "benchmark/fts/fts.wado"


def set_thresholds(o2_threshold: int, o3_threshold: int) -> None:
    with open(OPTIMIZE_RS) as f:
        content = f.read()
    # Replace O2|Os threshold
    content = re.sub(
        r'(OptLevel::O2 \| OptLevel::Os =>.*?inline_threshold: )\d+',
        lambda m: m.group(1) + str(o2_threshold),
        content,
        flags=re.DOTALL,
    )
    # Replace O3 threshold
    content = re.sub(
        r'(OptLevel::O3 =>.*?inline_threshold: )\d+',
        lambda m: m.group(1) + str(o3_threshold),
        content,
        flags=re.DOTALL,
    )
    with open(OPTIMIZE_RS, "w") as f:
        f.write(content)


def build() -> bool:
    result = subprocess.run(
        ["cargo", "build", "--release", "-p", "wado-cli"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        print(f"Build failed:\n{result.stderr}", file=sys.stderr)
        return False
    return True


def run_bench(bench_file: str, opt_level: str = "-O2") -> int | None:
    """Run benchmark and return elapsed ms. Returns None on failure."""
    result = subprocess.run(
        [WADO_BIN, "run", opt_level, bench_file],
        capture_output=True,
        text=True,
        timeout=120,
    )
    if result.returncode != 0:
        return None
    for line in result.stdout.splitlines():
        if "Elapsed:" in line:
            try:
                return int(line.split()[1])
            except (ValueError, IndexError):
                pass
    return None


def measure_total(opt_level: str = "-O2", runs: int = 2) -> int:
    """Measure total time (zlib + fts), take min over multiple runs."""
    results = []
    for _ in range(runs):
        zlib = run_bench(ZLIB_BENCH, opt_level)
        fts = run_bench(FTS_BENCH, opt_level)
        if zlib is not None and fts is not None:
            results.append(zlib + fts)
            print(f"    zlib={zlib}ms  fts={fts}ms  total={zlib+fts}ms")
    if not results:
        return 999999
    return min(results)


def test_threshold(o2: int, o3: int, label: str) -> int:
    print(f"\n--- threshold O2={o2} O3={o3} [{label}] ---")
    set_thresholds(o2, o3)
    if not build():
        return 999999
    # Measure at O2
    print("  Measuring -O2:")
    return measure_total("-O2")


def ternary_search_o2(o3_fixed: int, lo: int, hi: int) -> tuple[int, int]:
    """Ternary search for optimal O2 threshold in [lo, hi]. Returns (best_threshold, best_time)."""
    print(f"\n=== Ternary search O2 threshold [{lo}, {hi}] ===")
    best_t = (lo + hi) // 2
    best_time = 999999

    while hi - lo > 2:
        m1 = lo + (hi - lo) // 3
        m2 = hi - (hi - lo) // 3

        t1 = test_threshold(m1, o3_fixed, f"m1={m1}")
        t2 = test_threshold(m2, o3_fixed, f"m2={m2}")

        print(f"  m1={m1} time={t1}  m2={m2} time={t2}")

        if t1 <= t2:
            # Minimum is in [lo, m2]
            hi = m2
            if t1 < best_time:
                best_time = t1
                best_t = m1
        else:
            # Minimum is in [m1, hi]
            lo = m1
            if t2 < best_time:
                best_time = t2
                best_t = m2

    # Check remaining values
    for t in range(lo, hi + 1):
        time = test_threshold(t, o3_fixed, f"check={t}")
        if time < best_time:
            best_time = time
            best_t = t

    return best_t, best_time


def ternary_search_o3(o2_fixed: int, lo: int, hi: int) -> tuple[int, int]:
    """Ternary search for optimal O3 threshold in [lo, hi]. Returns (best_threshold, best_time)."""
    print(f"\n=== Ternary search O3 threshold [{lo}, {hi}] ===")
    best_t = (lo + hi) // 2
    best_time = 999999

    while hi - lo > 20:
        m1 = lo + (hi - lo) // 3
        m2 = hi - (hi - lo) // 3

        t1 = test_threshold(o2_fixed, m1, f"m1={m1}")
        t2 = test_threshold(o2_fixed, m2, f"m2={m2}")

        print(f"  m1={m1} time={t1}  m2={m2} time={t2}")

        if t1 <= t2:
            hi = m2
            if t1 < best_time:
                best_time = t1
                best_t = m1
        else:
            lo = m1
            if t2 < best_time:
                best_time = t2
                best_t = m2

    # Check a few values around the minimum
    step = max(1, (hi - lo) // 5)
    for t in range(lo, hi + 1, step):
        time = test_threshold(o2_fixed, t, f"check={t}")
        if time < best_time:
            best_time = time
            best_t = t

    return best_t, best_time


if __name__ == "__main__":
    # --- Phase 1: Tune O2 threshold (≤50) ---
    print("=" * 60)
    print("Phase 1: Tuning O2 threshold (range [10, 50])")
    print("=" * 60)

    # Current O3 threshold = 20 (unchanged during O2 tuning)
    best_o2, best_o2_time = ternary_search_o2(o3_fixed=20, lo=10, hi=50)
    print(f"\n>>> Best O2 threshold: {best_o2} (total={best_o2_time}ms)")

    # --- Phase 2: Tune O3 threshold ---
    print("\n" + "=" * 60)
    print("Phase 2: Tuning O3 threshold (range [20, 1000])")
    print("=" * 60)

    # Use best O2 threshold found above, search O3 in [20, 1000]
    # Measure at O3
    def measure_total_o3(o3: int) -> int:
        print(f"\n--- threshold O2={best_o2} O3={o3} [O3 tune] ---")
        set_thresholds(best_o2, o3)
        if not build():
            return 999999
        print("  Measuring -O3:")
        results = []
        for _ in range(2):
            zlib = run_bench(ZLIB_BENCH, "-O3")
            fts = run_bench(FTS_BENCH, "-O3")
            if zlib is not None and fts is not None:
                results.append(zlib + fts)
                print(f"    zlib={zlib}ms  fts={fts}ms  total={zlib+fts}ms")
        if not results:
            return 999999
        return min(results)

    # Ternary search O3
    lo, hi = 20, 1000
    best_o3 = (lo + hi) // 2
    best_o3_time = 999999

    while hi - lo > 20:
        m1 = lo + (hi - lo) // 3
        m2 = hi - (hi - lo) // 3

        t1 = measure_total_o3(m1)
        t2 = measure_total_o3(m2)

        print(f"  m1={m1} time={t1}  m2={m2} time={t2}")

        if t1 <= t2:
            hi = m2
            if t1 < best_o3_time:
                best_o3_time = t1
                best_o3 = m1
        else:
            lo = m1
            if t2 < best_o3_time:
                best_o3_time = t2
                best_o3 = m2

    step = max(1, (hi - lo) // 5)
    for t in range(lo, hi + 1, step):
        time = measure_total_o3(t)
        if time < best_o3_time:
            best_o3_time = time
            best_o3 = t

    print(f"\n>>> Best O3 threshold: {best_o3} (total={best_o3_time}ms)")

    # --- Final: Apply best thresholds ---
    print("\n" + "=" * 60)
    print(f"Final: O2={best_o2}, O3={best_o3}")
    print("=" * 60)
    set_thresholds(best_o2, best_o3)
    build()

    print("\nFinal verification:")
    print("-O2:")
    measure_total("-O2")
    print("-O3:")
    results = []
    for _ in range(2):
        zlib = run_bench(ZLIB_BENCH, "-O3")
        fts = run_bench(FTS_BENCH, "-O3")
        if zlib is not None and fts is not None:
            results.append(zlib + fts)
            print(f"    zlib={zlib}ms  fts={fts}ms  total={zlib+fts}ms")

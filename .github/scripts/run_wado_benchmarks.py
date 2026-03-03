#!/usr/bin/env python3
"""Run Wado-only runtime benchmarks and output JSON for github-action-benchmark."""

import json
import re
import subprocess
import sys

WADO = "./target/release/wado"


def run_bench(src: str) -> str:
    result = subprocess.run(
        [WADO, "run", "-O2", src],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def parse_ms(output: str, pattern: str = r"Elapsed: (\d+) ms") -> int:
    m = re.search(pattern, output)
    if not m:
        raise ValueError(f"Pattern {pattern!r} not found in: {output!r}")
    return int(m.group(1))


benchmarks = []

output = run_bench("benchmark/count_prime/count_prime.wado")
benchmarks.append({"name": "count_prime", "unit": "ms", "value": parse_ms(output)})

output = run_bench("benchmark/mandelbrot/mandelbrot.wado")
benchmarks.append({"name": "mandelbrot", "unit": "ms", "value": parse_ms(output)})

output = run_bench("benchmark/sieve/sieve.wado")
benchmarks.append({"name": "sieve", "unit": "ms", "value": parse_ms(output)})

output = run_bench("benchmark/fts/fts.wado")
benchmarks.append({"name": "fts", "unit": "ms", "value": parse_ms(output)})

output = run_bench("benchmark/zlib/zlib_bench.wado")
benchmarks.append({"name": "zlib/compress", "unit": "ms", "value": parse_ms(output, r"Compress: (\d+) ms")})
benchmarks.append({"name": "zlib/decompress", "unit": "ms", "value": parse_ms(output, r"Decompress: (\d+) ms")})

json.dump(benchmarks, sys.stdout, indent=2)
print()

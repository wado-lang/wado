window.BENCHMARK_DATA = {
  "lastUpdate": 1780351408760,
  "repoUrl": "https://github.com/wado-lang/wado",
  "entries": {
    "Benchmark": [
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "eab7ec5746d1fc1f9de0c2f28ff91c2e2ce1703c",
          "message": "Merge pull request #1264 from wado-lang/claude/crore-benchmark-metrics-GWhcl\n\nbenchmark: throughput metrics with ~1s auto-calibration",
          "timestamp": "2026-06-01T23:27:00+09:00",
          "tree_id": "f47630c3afdaaeaadba35a6e053c6bc28f2f59da",
          "url": "https://github.com/wado-lang/wado/commit/eab7ec5746d1fc1f9de0c2f28ff91c2e2ce1703c"
        },
        "date": 1780324546511,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 33.87,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.93,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.02,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.33,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 229.8,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 87.28,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 156.13,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.14,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.37,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 153.88,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.27,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.85,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.65,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 231.56,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 90.06,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 149.41,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.73,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.97,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 153.62,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.37,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 36.82,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.91,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 232.75,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 89.41,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 151.55,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.75,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.54,
            "unit": "MB/s"
          }
        ]
      },
      {
        "commit": {
          "author": {
            "email": "g.psy.va@gmail.com",
            "name": "FUJI Goro",
            "username": "gfx"
          },
          "committer": {
            "email": "noreply@github.com",
            "name": "GitHub",
            "username": "web-flow"
          },
          "distinct": true,
          "id": "ccb0e8a95bd237a73b677e0b94921f6ee49ad3ce",
          "message": "Merge pull request #1265 from wado-lang/claude/crore-benchmark-metrics-GWhcl\n\ndocs: point the runtime benchmark link at the throughput series",
          "timestamp": "2026-06-02T06:53:56+09:00",
          "tree_id": "6ca28d3363b28411238980e0cc4e71cfebfc5786",
          "url": "https://github.com/wado-lang/wado/commit/ccb0e8a95bd237a73b677e0b94921f6ee49ad3ce"
        },
        "date": 1780351408252,
        "tool": "customBiggerIsBetter",
        "benches": [
          {
            "name": "count_prime (-O1)",
            "value": 6.78,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O1)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O1)",
            "value": 36.49,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O1)",
            "value": 6.87,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O1)",
            "value": 18.05,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O1)",
            "value": 45.34,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O1)",
            "value": 230.42,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O1)",
            "value": 85.46,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O1)",
            "value": 156.79,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O1)",
            "value": 5.17,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O1)",
            "value": 2.38,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O2)",
            "value": 6.79,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O2)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O2)",
            "value": 154.15,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O2)",
            "value": 9.26,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O2)",
            "value": 36.68,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O2)",
            "value": 74.78,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O2)",
            "value": 233.97,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O2)",
            "value": 88.94,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O2)",
            "value": 150.82,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O2)",
            "value": 6.79,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O2)",
            "value": 2.93,
            "unit": "MB/s"
          },
          {
            "name": "count_prime (-O3)",
            "value": 6.81,
            "unit": "M numbers/s"
          },
          {
            "name": "mandelbrot (-O3)",
            "value": 4.84,
            "unit": "M px/s"
          },
          {
            "name": "sieve (-O3)",
            "value": 154.13,
            "unit": "M numbers/s"
          },
          {
            "name": "fts (-O3)",
            "value": 9.38,
            "unit": "M conversions/s"
          },
          {
            "name": "zlib/compress (-O3)",
            "value": 37.24,
            "unit": "MB/s"
          },
          {
            "name": "zlib/decompress (-O3)",
            "value": 74.87,
            "unit": "MB/s"
          },
          {
            "name": "json/twitter (-O3)",
            "value": 233.01,
            "unit": "MB/s"
          },
          {
            "name": "json/canada (-O3)",
            "value": 90.48,
            "unit": "MB/s"
          },
          {
            "name": "json/catalog (-O3)",
            "value": 148.63,
            "unit": "MB/s"
          },
          {
            "name": "sqlite_parse (-O3)",
            "value": 4.95,
            "unit": "MB/s"
          },
          {
            "name": "syntax_highlight (-O3)",
            "value": 2.55,
            "unit": "MB/s"
          }
        ]
      }
    ]
  }
}
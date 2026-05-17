---
name: profiling-wado
description: Profile Wado programs using wasmtime's guest profiler to identify hot functions.
---

# Profiling Wado Programs

Profile Wado programs with `--profile guest` to identify hot functions and call stacks.

## Workflow

### 1. Run with guest profiler

```sh
wado run --profile guest,profile.json,1 prog.wado
```

Parameters: `guest[,path[,interval_ms]]`

- `path`: output file (default: `profile.json`)
- `interval_ms`: sampling interval in milliseconds (default: 10, use 1 for short-running programs, use 0 for exhaustive recording)

Or with cargo during development:

```sh
cargo run --release --bin wado -- run --profile guest,profile.json,1 prog.wado
```

### 2. Analyze with a script

The output is Firefox Profiler JSON format. Parse it programmatically:

```sh
python3 -c "
import json, sys
from collections import Counter

with open('profile.json') as f:
    data = json.load(f)

t = data['threads'][0]
strings = t['stringArray']
samples = t['samples']
stack_table = t['stackTable']
frame_table = t['frameTable']
func_table = t['funcTable']

# Count inclusive samples per function
func_counts = Counter()
for stack_idx in samples['stack']:
    cur = stack_idx
    while cur is not None:
        frame_idx = stack_table['frame'][cur]
        func_idx = frame_table['func'][frame_idx]
        func_name = strings[func_table['name'][func_idx]]
        func_counts[func_name] += 1
        cur = stack_table['prefix'][cur]

total = samples['length']
print(f'Total samples: {total}')
print(f'{"Samples":>8s} {"Pct":>6s}  Function')
for name, count in func_counts.most_common():
    print(f'{count:8d} {100*count/total:5.1f}%  {name}')
"
```

### 3. View in Firefox Profiler (optional)

Upload `profile.json` to `https://profiler.firefox.com/` for interactive flame graph visualization.

## Choosing the sampling interval

| Program duration | Recommended interval | Expected samples |
| ---------------- | -------------------- | ---------------- |
| < 200 ms         | 1 ms                 | ~100–200         |
| 200 ms – 2 s     | 5 ms                 | ~40–400          |
| > 2 s            | 10 ms (default)      | 200+             |
| (exhaustive)     | 0 ms                 | every call       |

Use shorter intervals for short-running programs to get enough samples for meaningful analysis.

Setting `interval_ms` to `0` records every function entry/exit instead of sampling. This gives exact call counts but incurs ~10–15x overhead and produces large output files (~25 MB for 1M+ calls). Use only when precise call counts are needed.

## Characteristics

- **Cross-platform:** Works on Linux, macOS, and Windows
- **Runtime overhead:** ~5–8%
- **Granularity:** Function-level with call stacks
- **Output:** Firefox Profiler JSON (~5–50 KB)
- **CM-async compatibility:** Works (wasmtime 42+)

## Symbol naming

Function names reflect Wado source names with monomorphization detail:

```
f64::fmt_fixed
Status^Deserialize::deserialize<JsonDeserializer>
TreeMap::insert
```

## Cleanup

```sh
rm -f profile.json
```

## Notes

- For instruction-level profiling on Linux, use `--profile jitdump` with `perf`. See `docs/jitdump-profiling.md`.
- The `--profile perfmap` mode is also available for Linux `perf` integration without the `perf inject` step, but provides function-level granularity only.

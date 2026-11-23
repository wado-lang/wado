# wado-run-pjrt

`wado run-pjrt` runs a Wado program on a device behind a PJRT plugin: XLA's CPU
plugin by default, `libtpu.so` for a TPU, or any other `--plugin` names. It is
the host of `wado-lang:pjrt`, the package a program reaches the device through,
which this directory also holds.

```toml
[dependencies]
"wado-lang:pjrt" = { path = "../wado-run-pjrt" }
```

```wado
use { println, Stdout } from "core:cli";
use { PjrtHost } from "wado-lang:pjrt";

export fn run() with (Stdout, PjrtHost) {
    println(PjrtHost::platform().unwrap());   // "cpu" on XLA's CPU plugin
}
```

A program compiles StableHLO, moves buffers to and from the device, and runs
what it compiled; `wado doc` on `src/pjrt.wado` states each call. Under any
other host, a program runs until its first call of the package, which traps.

```sh
wado run-pjrt program.wado
wado run-pjrt --plugin libtpu.so program.wado
```

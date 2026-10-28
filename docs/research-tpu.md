# Google TPU Through PJRT — Research Reference

Date: 2026-10-28

How a program outside Python reaches a Google TPU, and how much of that can be
exercised on a machine that has no TPU. This document records findings only and
draws no conclusions about Wado.

> **Source**: the linked documentation, and the probe in the appendix, run
> against the binaries named in [Plugins](#3-plugins) on a 4-core x86-64 Linux
> container with no accelerator. Searched and measured 2026-10-28.

---

## Table of Contents

1. [Summary](#1-summary)
2. [The Only Public Entry Is PJRT](#2-the-only-public-entry-is-pjrt)
3. [Plugins](#3-plugins)
4. [The Program](#4-the-program)
5. [Without a TPU](#5-without-a-tpu)
6. [Shapes Must Be Static](#6-shapes-must-be-static)
7. [The Compiler's Cost Model](#7-the-compilers-cost-model)
8. [Routes That Do Not Reach a TPU](#8-routes-that-do-not-reach-a-tpu)
9. [Real TPUs](#9-real-tpus)
10. [Sources](#10-sources)
11. [Appendix: The Probe](#appendix-the-probe)

---

## 1. Summary

- A TPU has no public instruction set or driver API. What is public is
  `libtpu.so`, a PJRT plugin, which compiles a program with XLA and runs it.
  PyTorch/XLA's documentation names it as the Cloud TPU plugin. It takes a
  StableHLO module or a serialized HLO module.
- PJRT is a C API in one header. A plugin exports one symbol, `GetPjrtApi`,
  which returns a table of function pointers. Nothing else is linked, so a host
  in any language that can call C can drive it.
- `libtpu.so` installs from PyPI. On a machine with no TPU it loads, but it
  cannot create a client, so it runs nothing.
- `libtpu.so` still compiles for a TPU it does not have. Given a topology name
  such as `v5e:1x1`, it compiles a program for that TPU generation, reports the
  device memory the program needs, and estimates its run time from its cost
  model. The search found no published simulator of TPU execution, so this is
  as near to one as a machine without a TPU gets.
- An XLA CPU plugin runs the same StableHLO through the same API, on the CPU.
  ZML publishes it prebuilt.
- XLA on both plugins refuses a dimension of unknown extent. On `libtpu.so` the
  refusal is a failed assertion that aborts the whole process.

## 2. The Only Public Entry Is PJRT

PJRT is the interface between a framework and a device. The framework is the
client; a vendor ships the device side as a plugin, a shared library. The C API
(`xla/pjrt/c/pjrt_c_api.h` in openxla/xla) mirrors the C++ one.

Each call takes one argument struct, which opens with its own `struct_size`.
A plugin built against an older header reads the prefix it knows, so a client
and a plugin of different minor versions work together. The probe was built
against header 0.117 and ran against plugins reporting 0.114 and 0.116.

The calls a program needs to run on a device:

| Step                     | Call                                                       |
| ------------------------ | ---------------------------------------------------------- |
| Load                     | `dlopen`, then `GetPjrtApi`, then `PJRT_Plugin_Initialize` |
| Open the device          | `PJRT_Client_Create`, `PJRT_Client_AddressableDevices`     |
| Compile                  | `PJRT_Client_Compile`                                      |
| Copy an input in         | `PJRT_Client_BufferFromHostBuffer`                         |
| Run                      | `PJRT_LoadedExecutable_Execute`                            |
| Copy an output out       | `PJRT_Buffer_ToHostBuffer`                                 |
| Wait for any of the last | `PJRT_Event_Await`                                         |

Compiling without a device uses `PJRT_TopologyDescription_Create` and
`PJRT_Compile` instead of the client.

A compile takes its options as a serialized `CompileOptionsProto`. An empty one
is not a valid default: the CPU plugin aborts on its replica count of 0. The
smallest working one sets one replica and one partition, six bytes
(`1a 04 20 01 28 01`).

The Rust crate `pjrt` (and `pjrt-sys`) wraps this API. Its last release is
0.2.0, from 2024-09-19, which predates the header's current version.

## 3. Plugins

| Plugin      | Where it comes from                                                         | Download | Library | `GetPjrtApi` | PJRT  |
| ----------- | --------------------------------------------------------------------------- | -------- | ------- | ------------ | ----- |
| `libtpu.so` | PyPI `libtpu` 0.0.51, `libtpu-0.0.51-cp312-cp312-manylinux_2_31_x86_64.whl` | 205 MB   | 575 MB  | yes          | 0.116 |
| CPU         | GitHub `zml/pjrt-artifacts` release v18.0.0, `pjrt-cpu_linux-amd64.tar.gz`  | 67 MB    | 275 MB  | yes          | 0.114 |

The `libtpu` wheel holds no Python dependency; the library is the whole of it.
The ZML archive holds one file, `libpjrt_cpu.so`. ZML builds it from
openxla/xla's `//xla/pjrt/c:pjrt_c_api_cpu_plugin` target at a pinned commit,
with a few patches of its own, and also publishes CUDA, ROCm, oneAPI and macOS
builds.

PyPI carries NVIDIA GPU plugins, `jax-cuda12-pjrt` and `jax-cuda13-pjrt`,
which the probe did not run, and no CPU plugin. `jaxlib` 0.12.0.1 links XLA's
CPU client into `libjax_common.so`, which exports no `GetPjrtApi`.

## 4. The Program

A PJRT program is named by a format string, which `PJRT_Program` lists:
`"mlir"` takes a StableHLO module as MLIR bytecode or as text, `"hlo"` a
serialized `HloModuleProto`, and `"hlo_with_config"` an
`HloModuleProtoWithConfig`. The probe used `"mlir"` alone, and both plugins
accepted the same text module.

StableHLO promises compatibility only for a _portable artifact_, which is MLIR
bytecode written for a named StableHLO version. One build reads an artifact
another wrote with the same meaning if their commits are less than 5 years
apart (an older artifact) or less than 2 years apart (a newer one, using no
feature the older build lacks). Text carries no promise. Writing a portable
artifact takes libStablehlo: `stablehlo-translate --serialize`, or
`serializePortableArtifact` in C++.

## 5. Without a TPU

| What the probe tried                     | CPU plugin                        | `libtpu.so`                                           |
| ---------------------------------------- | --------------------------------- | ----------------------------------------------------- |
| Load and initialize                      | works                             | works, with `TPU_SKIP_MDS_QUERY=1` (see below)        |
| Create a client                          | works, 4 devices                  | fails: `No jellyfish device found`                    |
| Compile and run                          | works, result correct             | —                                                     |
| Compile for a topology (`PJRT_Compile`)  | refused: `Topology not supported` | works for `v4:2x2x1`, `v5e:1x1`, `v5e:2x2`, `v6e:1x1` |
| Serialize the compiled executable        | —                                 | works                                                 |
| Device memory and cost of the executable | —                                 | works                                                 |

```sh
./probe ./libpjrt_cpu.so affine.mlir run
./probe ./libpjrt_cpu.so affine.mlir aot cpu
TPU_SKIP_MDS_QUERY=1 ./probe ./libtpu.so affine.mlir run
TPU_SKIP_MDS_QUERY=1 ./probe ./libtpu.so gemv.mlir aot v4:2x2x1 v5e:1x1 v5e:2x2 v6e:1x1
```

`libtpu.so` asks the Compute Engine metadata server for the TPU's environment
when it initializes. With no metadata server it retries each key 30 times
before moving on. `TPU_SKIP_MDS_QUERY=1` skips the query.
A topology name must match `^([a-zA-Z0-9_ ]+)[=_:]([0-9x]+)(_twisted|_untwisted)?$`,
so a generation's marketing name such as `TPU v5 lite` is refused.

The run on the CPU plugin computed `tanh(x · w + b)` over `f32[2,3]`,
`f32[3,2]` and `f32[2,2]` (`affine.mlir` in the appendix). The result matched
the value computed by hand. The first execution includes one-time setup, so
the later ones are the cost of a call. Over three runs of the command above:

| Execution | Time, the third's as 1 |
| --------- | ---------------------- |
| first     | 10 to 16               |
| second    | 1.2 to 1.6             |
| third     | 1                      |

## 6. Shapes Must Be Static

XLA compiles for fixed extents. A dimension written `?` in StableHLO is
_unbounded dynamism_, and both plugins refuse it (`dynamic.mlir` in the
appendix):

| Plugin      | What happens                                                                                   |
| ----------- | ---------------------------------------------------------------------------------------------- |
| CPU         | `PJRT_Client_Compile` returns an error: `Unbounded dynamism is disabled for instruction`       |
| `libtpu.so` | `PJRT_Compile` fails the assertion `!count_overflow` in `shape_util.cc` and aborts the process |

```sh
./probe ./libpjrt_cpu.so dynamic.mlir run
TPU_SKIP_MDS_QUERY=1 ./probe ./libtpu.so dynamic.mlir aot v5e:1x1
```

A program that varies an extent compiles once per extent.

## 7. The Compiler's Cost Model

A compiled TPU executable reports, through `PJRT_Executable_GetCostAnalysis`,
the floating-point operations and bytes it moves, and `optimal_seconds`: the
compiler's estimate of its run time on that generation.
`PJRT_Executable_GetCompiledMemoryStats` reports the device memory it needs.

The program below is a matrix-vector product the size of GPT-2's output layer:
a `bf16[1,768]` row times a `bf16[768,50257]` matrix, accumulated in `f32`
(`gemv.mlir` in the appendix). Its arguments take 77,208,576 bytes of device
memory. The cost analysis reports the same on every topology: `bytes accessed`
of 7.78276 × 10⁷ and `flops` of 1.15743 × 10⁸, about 1.5 per byte, so the
program is bound by memory bandwidth. Its effective bandwidth is
`bytes accessed` divided by `optimal_seconds`.

| Topology   | `optimal_seconds` | Effective bandwidth |
| ---------- | ----------------- | ------------------- |
| `v4:2x2x1` | 1.58432 × 10⁻⁴    | 491 GB/s            |
| `v5e:1x1`  | 1.05506 × 10⁻⁴    | 738 GB/s            |
| `v5e:2x2`  | 1.05506 × 10⁻⁴    | 738 GB/s            |
| `v6e:1x1`  | 4.75138 × 10⁻⁵    | 1638 GB/s           |

```sh
TPU_SKIP_MDS_QUERY=1 ./probe ./libtpu.so gemv.mlir aot v4:2x2x1 v5e:1x1 v5e:2x2 v6e:1x1
```

`flops` is XLA's count. A product of one row by a 768 × 50257 matrix is
3.86 × 10⁷ multiply-adds, and XLA's count is 2.9987 times that, which the
research did not explain. `optimal_seconds` is the compiler's model, not a
measurement on a device.

The first compile in a process includes the compiler's one-time setup. The
compiles after it took within 1.3 times of one another.

## 8. Routes That Do Not Reach a TPU

- wasi-nn: its `graph-encoding` enum is `openvino`, `onnx`, `tensorflow`,
  `pytorch`, `tensorflowlite`, `ggml` and `autodetect`. StableHLO is not among
  them. wasmtime's implementation (`crates/wasi-nn/src/backend/` in
  `vendor/wasmtime`) has OpenVINO, ONNX Runtime, PyTorch and WinML backends.
- Pallas TPU interpret mode: JAX runs a Pallas TPU kernel on the CPU,
  simulating the TPU's memories, DMAs and semaphores in Python. It serves
  kernels written in Pallas, from Python, and runs no XLA program.
- Open TPU-like designs: OpenTPU (UCSB) reimplements a TPU in RTL with an
  instruction set, simulator and compiler of its own, and tiny-tpu is a
  teaching-scale systolic array. Neither runs a program `libtpu.so` compiles.

## 9. Real TPUs

`libtpu.so` creates a client only on a TPU VM.

- Google Colab offers a TPU runtime in the browser, at no charge.
- Kaggle notebooks offer a TPU v5e with 8 chips, by a secondary source.
- TPU Research Cloud gives accepted researchers Cloud TPUs at no charge.
- Cloud TPU rents a TPU VM by the hour.

Execution on a TPU was not measured. The probe's `run` mode is what measures
it, and needs one of these.

## 10. Sources

- [PJRT integration](https://openxla.org/xla/pjrt/pjrt_integration), and
  [`pjrt_c_api.h`](https://github.com/openxla/xla/blob/main/xla/pjrt/c/pjrt_c_api.h)
- [PJRT C++ Device API overview](https://openxla.org/xla/pjrt/cpp_api_overview)
- [PJRT examples](https://openxla.org/xla/pjrt/examples)
- [StableHLO compatibility](https://openxla.org/stablehlo/compatibility)
- [PyPI: libtpu](https://pypi.org/project/libtpu/), [PyPI: jaxlib](https://pypi.org/project/jaxlib/),
  [PyPI: jax-cuda12-pjrt](https://pypi.org/project/jax-cuda12-pjrt/)
- [zml/pjrt-artifacts](https://github.com/zml/pjrt-artifacts)
- [crates.io: pjrt](https://crates.io/crates/pjrt)
- [pypjrt](https://pypi.org/project/pypjrt/), a Python PJRT client
- [wasi-nn WIT](https://github.com/WebAssembly/wasi-nn/blob/main/wit/wasi-nn.wit),
  [wasmtime-wasi-nn backends](https://docs.wasmtime.dev/api/wasmtime_wasi_nn/backend/index.html)
- [Pallas TPU interpret mode](https://docs.jax.dev/en/latest/_autosummary/jax.experimental.pallas.tpu.force_tpu_interpret_mode.html)
- [OpenTPU](https://github.com/UCSBarchlab/OpenTPU)
- [Cloud TPU Colabs](https://cloud.google.com/tpu/docs/colabs),
  [TPU Research Cloud](https://sites.research.google/trc/about/),
  [Kaggle TPU v5e (secondary)](https://medium.com/@strzelczyk/tpu-mythbusting-cost-and-usage-1ca96c85df94)

## Appendix: The Probe

To reproduce, put `pjrt_c_api.h` from openxla/xla beside `probe.c`, then:

```sh
gcc -O2 -o probe probe.c -ldl
curl -LO https://github.com/zml/pjrt-artifacts/releases/download/v18.0.0/pjrt-cpu_linux-amd64.tar.gz
tar xzf pjrt-cpu_linux-amd64.tar.gz
pip download --no-deps libtpu==0.0.51 && unzip -j libtpu-*.whl libtpu/libtpu.so
```

`probe.c`:

```c
// probe <plugin.so> <program.mlir> run              compile and run on the plugin's device
// probe <plugin.so> <program.mlir> aot <topology>... compile for a topology, with no device
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "pjrt_c_api.h"

// Every argument struct of the C API opens with its own size.
#define ARGS(T, name, ...) T name = {.struct_size = T##_STRUCT_SIZE, __VA_ARGS__}

static const PJRT_Api* api;

static void check(PJRT_Error* e, const char* what) {
  if (!e) return;
  ARGS(PJRT_Error_Message_Args, m, .error = e);
  api->PJRT_Error_Message(&m);
  fprintf(stderr, "%s: %.*s\n", what, (int)m.message_size, m.message);
  exit(1);
}

static void await_event(PJRT_Event* ev) {
  ARGS(PJRT_Event_Await_Args, aw, .event = ev);
  check(api->PJRT_Event_Await(&aw), "Event_Await");
}

static double now_us(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

// CompileOptionsProto { executable_build_options { num_replicas: 1, num_partitions: 1 } }.
// An empty one leaves the replica count at 0, which the CPU plugin aborts on.
static const char options[] = "\x1a\x04\x20\x01\x28\x01";

static void report(PJRT_Executable* exe) {
  ARGS(PJRT_Executable_GetCompiledMemoryStats_Args, mem, .executable = exe);
  check(api->PJRT_Executable_GetCompiledMemoryStats(&mem), "GetCompiledMemoryStats");
  printf("  device bytes: arguments %ld, outputs %ld, temporaries %ld\n", (long)mem.argument_size_in_bytes,
         (long)mem.output_size_in_bytes, (long)mem.temp_size_in_bytes);
  ARGS(PJRT_Executable_GetCostAnalysis_Args, cost, .executable = exe);
  check(api->PJRT_Executable_GetCostAnalysis(&cost), "GetCostAnalysis");
  for (size_t i = 0; i < cost.num_properties; i++) {
    const PJRT_NamedValue* p = &cost.properties[i];
    if (p->type == PJRT_NamedValue_kFloat) printf("  %.*s = %g\n", (int)p->name_size, p->name, p->float_value);
  }
  ARGS(PJRT_Executable_Serialize_Args, ser, .executable = exe);
  check(api->PJRT_Executable_Serialize(&ser), "Serialize");
  printf("  serialized: %zu bytes\n", ser.serialized_bytes_size);
  ser.serialized_executable_deleter(ser.serialized_executable);
}

static void aot(const PJRT_Program* program, int count, char** topologies) {
  for (int i = 0; i < count; i++) {
    ARGS(PJRT_TopologyDescription_Create_Args, topo, .topology_name = topologies[i],
         .topology_name_size = strlen(topologies[i]));
    check(api->PJRT_TopologyDescription_Create(&topo), topologies[i]);
    double t0 = now_us();
    ARGS(PJRT_Compile_Args, comp, .topology = topo.topology, .program = program, .compile_options = options,
         .compile_options_size = sizeof options - 1);
    check(api->PJRT_Compile(&comp), "Compile");
    printf("%s: compiled in %.0f us\n", topologies[i], now_us() - t0);
    report(comp.executable);
  }
}

// The program run takes f32[2,3], f32[3,2] and f32[2,2], and returns one f32[2,2].
static void run(const PJRT_Program* program) {
  ARGS(PJRT_Client_Create_Args, cc);
  check(api->PJRT_Client_Create(&cc), "Client_Create");
  ARGS(PJRT_Client_AddressableDevices_Args, devs, .client = cc.client);
  check(api->PJRT_Client_AddressableDevices(&devs), "AddressableDevices");

  ARGS(PJRT_Client_Compile_Args, comp, .client = cc.client, .program = program, .compile_options = options,
       .compile_options_size = sizeof options - 1);
  check(api->PJRT_Client_Compile(&comp), "Compile");

  float x[6] = {1, 2, 3, 4, 5, 6}, w[6] = {0.1f, 0.2f, 0.3f, 0.4f, 0.5f, 0.6f}, bias[4] = {-2, -3, -5, -6};
  const float* data[3] = {x, w, bias};
  int64_t dims[3][2] = {{2, 3}, {3, 2}, {2, 2}};
  PJRT_Buffer* inputs[3];
  for (int i = 0; i < 3; i++) {
    ARGS(PJRT_Client_BufferFromHostBuffer_Args, up, .client = cc.client, .data = data[i],
         .type = PJRT_Buffer_Type_F32, .dims = dims[i], .num_dims = 2,
         .host_buffer_semantics = PJRT_HostBufferSemantics_kImmutableOnlyDuringCall,
         .device = devs.addressable_devices[0]);
    check(api->PJRT_Client_BufferFromHostBuffer(&up), "BufferFromHostBuffer");
    await_event(up.done_with_host_buffer);
    inputs[i] = up.buffer;
  }

  PJRT_Buffer* const* input_lists[1] = {inputs};
  PJRT_Buffer* outputs[1];
  PJRT_Buffer** output_lists[1] = {outputs};
  ARGS(PJRT_ExecuteOptions, opts);
  for (int i = 0; i < 3; i++) {
    PJRT_Event* done[1];
    double t0 = now_us();
    ARGS(PJRT_LoadedExecutable_Execute_Args, ex, .executable = comp.executable, .options = &opts,
         .argument_lists = input_lists, .num_devices = 1, .num_args = 3, .output_lists = output_lists,
         .device_complete_events = done);
    check(api->PJRT_LoadedExecutable_Execute(&ex), "Execute");
    await_event(done[0]);
    printf("execute %d: %.1f us\n", i, now_us() - t0);
  }

  float r[4];
  ARGS(PJRT_Buffer_ToHostBuffer_Args, down, .src = outputs[0], .dst = r, .dst_size = sizeof r);
  check(api->PJRT_Buffer_ToHostBuffer(&down), "ToHostBuffer");
  await_event(down.event);
  printf("result [[%g, %g], [%g, %g]]\n", r[0], r[1], r[2], r[3]);
}

int main(int argc, char** argv) {
  void* lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
  if (!lib) {
    fprintf(stderr, "%s\n", dlerror());
    return 1;
  }
  const PJRT_Api* (*get_api)(void) = (const PJRT_Api* (*)(void))dlsym(lib, "GetPjrtApi");
  if (!get_api) {
    fprintf(stderr, "%s exports no GetPjrtApi\n", argv[1]);
    return 1;
  }
  api = get_api();
  printf("PJRT %d.%d\n", api->pjrt_api_version.major_version, api->pjrt_api_version.minor_version);
  ARGS(PJRT_Plugin_Initialize_Args, init);
  check(api->PJRT_Plugin_Initialize(&init), "Plugin_Initialize");

  FILE* f = fopen(argv[2], "rb");
  if (!f) {
    perror(argv[2]);
    return 1;
  }
  fseek(f, 0, SEEK_END);
  size_t code_size = ftell(f);
  rewind(f);
  char* code = malloc(code_size);
  if (fread(code, 1, code_size, f) != code_size) {
    perror(argv[2]);
    return 1;
  }
  PJRT_Program program = {.struct_size = PJRT_Program_STRUCT_SIZE, .code = code, .code_size = code_size,
                          .format = "mlir", .format_size = 4};

  if (strcmp(argv[3], "aot") == 0)
    aot(&program, argc - 4, argv + 4);
  else
    run(&program);
  return 0;
}
```

`affine.mlir`:

```mlir
module @m {
  func.func public @main(%x: tensor<2x3xf32>, %w: tensor<3x2xf32>, %b: tensor<2x2xf32>) -> tensor<2x2xf32> {
    %0 = stablehlo.dot_general %x, %w, contracting_dims = [1] x [0] : (tensor<2x3xf32>, tensor<3x2xf32>) -> tensor<2x2xf32>
    %1 = stablehlo.add %0, %b : tensor<2x2xf32>
    %2 = stablehlo.tanh %1 : tensor<2x2xf32>
    return %2 : tensor<2x2xf32>
  }
}
```

`gemv.mlir`:

```mlir
module @m {
  func.func public @main(%x: tensor<1x768xbf16>, %w: tensor<768x50257xbf16>) -> tensor<1x50257xf32> {
    %0 = stablehlo.dot_general %x, %w, contracting_dims = [1] x [0] : (tensor<1x768xbf16>, tensor<768x50257xbf16>) -> tensor<1x50257xf32>
    return %0 : tensor<1x50257xf32>
  }
}
```

`dynamic.mlir`:

```mlir
module @m {
  func.func public @main(%a: tensor<?x256xf32>, %b: tensor<256x512xf32>) -> tensor<?x512xf32> {
    %0 = stablehlo.dot_general %a, %b, contracting_dims = [1] x [0] : (tensor<?x256xf32>, tensor<256x512xf32>) -> tensor<?x512xf32>
    return %0 : tensor<?x512xf32>
  }
}
```

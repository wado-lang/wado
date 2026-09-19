# WEP: `wasi:webgpu` Bindings

## Context

Wado targets WASI P3 ([Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)),
where a program reaches files, sockets, clocks and randomness but has no way to
reach a GPU. `wasi:webgpu` is the WASI proposal for that: Phase 2, portable to
Linux, Windows, macOS, Android and the Web, one WIT package at `0.3.0-rc.2`.

The proposal is the WebGPU spec with its JS assumptions removed, generated from
the WebGPU IDL. One file, one interface `webgpu`, and inside it 33 resources, 62
records, 34 enums, 12 variants, 5 flags, 13 typedefs, 8 `async func`, and a
single free function, `get-gpu`. Presenting to a screen is a declared non-goal
of the proposal, so what it covers is GPU compute and offscreen rendering.

Two things it needs from the toolchain are already there. `wado-from-idl` reads
WIT and emits a `wasi:*` module, and a Wado module may bind a Component Model
import in any namespace, with `#[cm]` naming the target
([Declaration Identity](./wep-2026-08-12-declaration-identity.md)).

## Decision

### The module is generated from the proposal's WIT

`wasi:*` is generated, never hand-written (`wado-from-idl/AGENTS.md`), and this
package is WIT like the rest, so whatever carries the module generates it.
wasmtime does not ship this WIT, so it comes from a `vendor/wasi-webgpu`
submodule, and `mise run update-stdlib-webgpu` regenerates from there.

Run against `0.3.0-rc.2`, the WIT frontend emits the whole interface as 2423
lines. It skips nothing and approximates nothing.

### The interface is the effect, and each resource is one too

A WIT interface's free functions become a Wado `interface`, so `webgpu` yields
`Webgpu` with `get_gpu()`, the one operation that hands out a handle. Effects
and resources are unified, so a program names every resource it touches in its
own `with` clause:

```wado
use { Webgpu, Gpu, GpuAdapter, GpuDevice, GpuBuffer } from "wasi:webgpu";

export fn run() with (Webgpu, Gpu, GpuAdapter, GpuDevice, GpuBuffer) {
    let gpu = Webgpu::get_gpu();
    let Some(adapter) = gpu.request_adapter(null).wait() else { return; };
    let Ok(device) = adapter.request_device(null).wait() else { return; };
    let buffer = device.create_buffer(GpuBufferDescriptor {
        size: 256,
        usage: GpuBufferUsage::Storage | GpuBufferUsage::CopySrc,
        mapped_at_creation: null,
        label: null,
    });
}
```

### An async operation returns `AsyncCall<T>`

Eight operations are `async func`: `request-adapter`, `request-device`,
`map-async`, the two `create-*-pipeline-async`, `pop-error-scope`,
`on-submitted-work-done` and `get-compilation-info`. Each follows
[Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md), so the call site
`.wait()`s. `gpu-device` also hands out a `future` and a `stream`: `lost()`
answers `Future<GpuDeviceLostInfo>` and `on_uncaptured_error()` a
`Stream<GpuError>`.

### The handles are affine CM resources

`web:dom` collapses its handles into one unrestricted universal handle
([Resource Inheritance](./wep-2026-04-28-resource-inheritance.md)). Nothing of
the sort applies here: every `wasi:webgpu` resource is a CM `resource`, so
[Ownership Analysis](./wep-2026-05-21-resource-ownership.md) governs it and the
boundary carries `own` and `borrow` handles as it does for any other WASI
package. The proposal borrows in 41 places, twelve of them record fields, so a
descriptor is a struct with reference-typed fields:

```wado
pub struct GpuBindGroupDescriptor {
    pub layout: &GpuBindGroupLayout,
    pub entries: List<GpuBindGroupEntry>,
    pub label: Option<String>,
}
```

`gpu-queue.submit` and `gpu-render-pass-encoder.execute-bundles` take
`list<borrow<t>>`, which is a `List<&T>`: the element is a borrow at the
boundary and a reference on the Wado side.

### What the boundary already carries

Compiled against the generated module, these shapes reach a valid component:
the descriptor above, a variant payload holding a record that holds a borrow
(`GpuBindingResource::GpuBufferBinding`), a `List<&T>` argument, an `async`
operation through `.wait()`, and a `flags` value built with `|`.

## Roadmap

- [ ] Vendor the proposal at `vendor/wasi-webgpu` and add
      `mise run update-stdlib-webgpu`, so the module regenerates like the rest.
- [ ] Generate `wado-compiler/lib/wasi/webgpu/` and register `wasi:webgpu` in
      the binding table, which also reserves the namespace.
- [ ] E2E fixtures for the boundary shapes the package leans on: a descriptor
      carrying a borrow, a `list<borrow<t>>` submit, and an `async` request.
      They are `compile_only` until a host exists.
- [ ] A `wasi:webgpu` section in [the WASI reference](./stdlib-wasi.md), with a
      compute walkthrough from `get_gpu` to a dispatched pass.

## Known gaps

- Whether the package is bundled at all. Every other `wasi:*` is, and bundling
  is what reserves the namespace. Against that, the module is 2423 lines of a
  Phase 2 proposal most programs never import, and `#[cm]` in any namespace lets
  an external package carry it instead. The first two roadmap items assume
  bundling and would be dropped if it were refused.
- What bundling costs is measured, against the module generated from
  `0.3.0-rc.2`. The registry bootstrap each `wado` process runs grows by the 8 ms
  the module takes to lex and parse, and each compilation's private copy of that
  registry by 0.9 ms, so the 13537-fixture e2e suite gains about 12 s of its
  1608 s. `mise run test-stdlib` gains three files and stays inside its own
  run-to-run spread, `wado format` 79 ms, and a release binary the module's
  87 KB of source. The vendored proposal is 692 KB beside wasmtime's 93 MB.
  Nothing here needs a mechanism to stay cheap; what is unmeasured is the same
  module once the LSP ships it to a browser.
- The bootstrap parses every bundled binding module, and each compilation copies
  the registry it fills, so both scale with what is bundled rather than with what
  a program imports — about 1.6 ms per compilation at today's sizes. Making
  either demand-driven means running the two-pass `use` resolution over a
  package closure instead of the whole table.
- Running needs a host `wado run` does not have. wasmtime ships none, but
  `wasi-gfx/wasi-gfx-runtime` does, as the `wasi-webgpu-wasmtime` crate, against
  the same `wasi:webgpu@0.3.0-rc.2` the module is generated from. A Wado
  component built on that module runs a WGSL compute dispatch through it and
  reads the results back. Two things stand between that and `wado run`: the
  crate requires wasmtime 48 where the workspace pins 47.0.3, and the GPU stack
  behind it is 31 crates — naga, wgpu-core, wgpu-hal and ash among them — about
  38 s of a clean release build and 3.1 MB of the binary. `wado` does not carry
  that: the host ships as `wado-run-webgpu`, reached through
  [External Subcommands](./wep-2026-09-19-external-subcommands.md), as a native
  binary, which the GPU stack leaves no choice about. What the binary is built
  from, and how it tracks the workspace's wasmtime pin, is open.
- A machine with no GPU has no adapter, and wgpu's `noop` backend is opt-in and
  computes nothing. `mesa-vulkan-drivers` supplies a software adapter
  (lavapipe), a 98.5 MB install, and that is what answered the compute run
  above. A CI job that runs rather than compiles a `wasi:webgpu` fixture needs
  it installed.
- The version rides in every `#[cm]` path, so an `rc.3` rewrites all 2423 lines.
  Regenerating handles that. What nothing records is which version the bundled
  module was cut from, beyond the submodule the generated header names.
- Presenting to a screen is outside the proposal. `gpu-canvas-context` is
  declared but nothing hands one out, so a program can render to a texture and
  not to a window. The proposal points at `wasi-gfx` for the missing half.
- Every data path copies: `write-buffer-with-copy`, `write-texture-with-copy`
  and `get-mapped-range-get-with-copy` take or return a `list<u8>`, so a buffer
  upload crosses linear memory twice. This is the proposal's shape, not the
  binding's.
- WebGPU's two `record<DOMString, V>` maps arrive as resources with
  `add`/`get`/`has`/`remove` rather than as a `TreeMap<String, V>`, so a
  pipeline's constant overrides are built through handle calls.
- A typedef becomes a newtype, so `buffer.size()` answers `GpuSize64Out` and
  needs `as u64` to meet a `u64`. 13 typedefs carry this, most of them widths.
- An `option<descriptor>` parameter takes an explicit `Option::Some(…)`, since
  the compiler rejects a default argument on a `#[cm]` operation
  ([WebIDL Binding Generator](./wep-2026-04-01-tide.md) records the same gap).
  WebGPU makes most descriptors optional, so this is the common call shape.

# WEP: `wasi:webgpu` Bindings

## Context

Wado targets WASI P3 ([Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)),
and GPU access is the one capability in that generation with no Wado surface.
`wasi:webgpu` is the WASI proposal for it: Phase 2, portability criteria Linux,
Windows, macOS, Android and Web, one WIT package at `0.3.0-rc.2`.

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
package is WIT like the rest. wasmtime does not carry it, so the WIT comes from
a `vendor/wasi-webgpu` submodule of its own, and `mise run update-stdlib-webgpu`
regenerates `wado-compiler/lib/wasi/webgpu/` beside the other packages.

Run against `0.3.0-rc.2`, the WIT frontend emits the whole interface — 2423
lines, nothing skipped and nothing approximated.

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

The eight `async func`s — `request-adapter`, `request-device`, `map-async`, the
two `create-*-pipeline-async`, `pop-error-scope`, `on-submitted-work-done`,
`get-compilation-info` — follow
[Generic `AsyncCall<T>`](./wep-2026-04-22-subtask-generic.md), so the call site
`.wait()`s. `gpu-device` also hands out a `future` and a `stream`: `lost()`
answers `Future<GpuDeviceLostInfo>` and `on_uncaptured_error()` a
`Stream<GpuError>`.

### The handles are affine CM resources

Unlike `web:dom`, whose handles collapse to one unrestricted universal handle
([Resource Inheritance](./wep-2026-04-28-resource-inheritance.md)), every
`wasi:webgpu` resource is a CM `resource`, so
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

`gpu-queue.submit` and `gpu-render-pass-encoder.execute-bundles` go further and
take `list<borrow<t>>`, which is a `List<&T>`: the element is a borrow at the
boundary and a reference on the Wado side.

### What the boundary already carries

Compiled against the generated module, these shapes reach a valid component:
the descriptor above, a variant payload holding a record that holds a borrow
(`GpuBindingResource::GpuBufferBinding`), a `List<&T>` argument, an `async`
operation through `.wait()`, and a `flags` value built with `|`.

Three compiler defects stood between them and that, each fixed with its own
fixture and none of them specific to this package: a CM import's return type,
applied from the call site, claimed every `i32` intermediate the adapter's body
held — a list parameter's own length among them; a `list<borrow<t>>` parameter
read the caller's list as a `List<i32>`, which is a different GC type from the
`List<&T>` the caller holds; and a borrow minted inside a list wrapped the
resource's `own` handle rather than the resource.

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

- Whether the package is bundled at all. Every other `wasi:*` is, and the
  namespace reservation follows from bundling; against that, the module is 2423
  lines of a proposal at Phase 2 that most programs never import, and
  `#[cm]` in any namespace means an external package could carry it instead.
  Unowned, and the choice decides the roadmap's first two items.
- No host implements `wasi:webgpu`, wasmtime included, so nothing the fixtures
  cover can be run — only compiled. Closing this means a host of our own or a
  third-party runtime to point `wado run` at.
- The version rides in every `#[cm]` path, so an `rc.3` rewrites all 2423
  lines. Regeneration handles it; a program that pinned nothing does not notice.
  Nothing tracks which version the bundled module was cut from beyond the
  generated header.
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
- An `option<descriptor>` parameter needs an explicit `Option::Some(…)`: the
  compiler rejects a default argument on a `#[cm]` operation
  ([WebIDL Binding Generator](./wep-2026-04-01-tide.md) records the same gap),
  so `create_command_encoder(null)` is how a caller says "no descriptor".

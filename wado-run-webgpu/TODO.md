# wado-run-webgpu TODO

- [ ] [A write from a file into a buffer](#a-write-from-a-file-into-a-buffer),
  undecided.

## A write from a file into a buffer

A host function of `wado run-webgpu`'s own, beside `wasi:webgpu`, would write a
byte range of a file into a `GpuBuffer` at an offset. The host maps the file,
so the bytes come from the page cache, and copies them once, into wgpu's
staging.

Loam's `WebGpuBackend` writes the experts its pools lack into slots, and on
gpt-oss-20b those writes take most of a token's time
([Measured on gpt-oss-20b](../package-loam/docs/cpu-experts.md#measured-on-gpt-oss-20b)).
Each write copies the bytes three times: out of the guest's `LinearArray64`
into a `LinearArray`, into the `Vec` wasmtime lifts `write-buffer`'s
`list<u8>` into, and into wgpu's staging. `memmove` takes about half the host's
time ([To measure](../package-loam/docs/cpu-experts.md#to-measure)). Writing
from the file also drops the guest's copy of every shared expert, which takes
most of the process's memory and much of its load time.

What it costs:

- It is not `wasi:webgpu`, so a program using it runs only here, as
  `wado-lang:pjrt` runs only under `wado run-pjrt`. Loam keeps the write from
  guest memory for every other host.
- The experts the host computes still need their weights in the guest. Either
  the guest keeps them, or every miss is written and none computed.
- The file range is a guest's claim about a host file, so the host checks it
  against the file's length and the guest's preopens before mapping it.

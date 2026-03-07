# DWARF Metadata for Wado

This document describes the plan for adding DWARF debug metadata to the Wado compiler output,
enabling source-level location mapping (file paths, line numbers) for Wado programs.

## Current Baseline

Wado already emits a Wasm **Name Section** (`WirNames` / `emit_name_section` in `codegen/emit.rs`),
which provides human-readable function names such as `Point::sum` or `Point^Display::fmt`.

**Verified**: Running `wado run --profile guest benchmark/fts/fts.wado` and inspecting the
resulting Firefox Profiler JSON confirms that symbol names are already fully resolved — all
16 functions appear with names like `f64::fmt_into`, `run`, `write_digits_at`, etc.
No `<unknown>` entries appear in the sampled stacks.

The compiler also already has all the source information needed in WIR:

| Information                | Source in WIR                                                           |
| -------------------------- | ----------------------------------------------------------------------- |
| Function display name      | `WirFunction.name.display` (e.g., `Point::sum`)                         |
| Source file path           | `WirFunction.meta.module_source` (`ModuleSource`)                       |
| Function start line/column | `WirFunction.meta.span` (`Span { line, column, start, end, end_line }`) |
| `gimli` crate              | Already in `Cargo.lock` v0.32.3 (transitive via wasmtime)               |

What the current baseline **cannot** do:

| Gap                                     | Impact                                                                          |
| --------------------------------------- | ------------------------------------------------------------------------------- |
| No source file/line in panic backtraces | Wasmtime prints `wasm backtrace` with function names only; no `src/foo.wado:42` |
| No source file/line in profiler frames  | Firefox Profiler / perf show function names but cannot link to source lines     |
| No debugger support                     | LLDB/GDB cannot step through Wado source code                                   |

## Benefits of Adding DWARF

### Benefit 1: Source Locations in Panic Backtraces (High Value, Phase 1)

Without DWARF, a Wasm trap prints only function names:

```
wasm backtrace:
    0: run
    1: wasi:cli/run#run
```

With DWARF (`DW_TAG_subprogram` with `DW_AT_decl_file` / `DW_AT_decl_line`), wasmtime's
`--wasm-backtrace-details=on` resolves each frame to a source location:

```
wasm backtrace:
    0: 0x1a2b - run
                    at src/main.wado:15
    1: 0x1c3d - wasi:cli/run#run
```

This is the most immediately actionable benefit. It requires only Phase 1 (function-level DWARF)
and no changes to WIR instruction representation.

### Benefit 2: Source-Level Profiling (Medium Value, Phase 2)

The current `--profile guest` output in Firefox Profiler shows correct function names but cannot
drill down to which line within a function is hot. With a `.debug_line` section (Phase 2),
profilers can display per-line attribution.

This requires attaching `Span` to each `WirInstr`, which is a larger WIR change.

### Benefit 3: Source-Level Debugging (Future Value, Phase 1+)

With `DW_TAG_subprogram` DIEs, LLDB and GDB can set function-level breakpoints and show the
current source file and line during a Wasm debugging session. This benefit compounds with Phase 2
for statement-level stepping.

## DWARF in Wasm

DWARF is embedded as custom sections in the Wasm binary:

| Section         | Role                                                       |
| --------------- | ---------------------------------------------------------- |
| `.debug_abbrev` | Abbreviation table for DIE encoding                        |
| `.debug_str`    | Interned string table (file names, function names)         |
| `.debug_info`   | Compilation unit and subprogram (function) DIEs            |
| `.debug_line`   | Line number program: instruction byte offset → source line |

These are added via `wasm_encoder::RawSection` after the core module is emitted.

Wasm-specific DWARF uses **code section byte offsets** (not virtual addresses) as program counters.
This is standardized in DWARF 5 with the `DW_AT_address_class` = 1 (code) attribute for Wasm.

## Implementation Plan

### Phase 1: Function-Level DWARF (Minimum Viable)

Emit one `DW_TAG_subprogram` DIE per function, with:

- `DW_AT_name`: function display name
- `DW_AT_decl_file`: source file path
- `DW_AT_decl_line`: function start line
- `DW_AT_low_pc` / `DW_AT_high_pc`: function byte range in code section

**How to get function byte offsets**: after `emit_core_module()` returns the raw bytes,
re-parse the code section with `wasmparser::CodeSectionReader` to find each function's
byte range. Match against `WirModule.functions` by index.

This phase does not require any changes to `WirInstr` or `WirMeta`.

### Phase 2: Instruction-Level Line Table (Future)

For fine-grained source mapping, add `Span` to each `WirInstr` variant (or a separate
`WirInstrWithSpan` wrapper). During emission in `emit_instr`, record
`(source_line, byte_offset)` pairs. Use these to build a compact `.debug_line`
line number program via `gimli::write::LineProgram`.

This requires changes to `WirInstr`, `wir_build/translate.rs`, and `codegen/emit.rs`.

### Phase 3: Type Information (Not Planned)

Wasm GC types (structs, variants) have no linear memory representation.
Standard DWARF type DIEs do not apply. Skip unless a specific debugger integration requires it.

## Implementation Details

### New File: `wado-compiler/src/codegen/dwarf.rs`

```
pub struct DwarfInput<'a> {
    pub wir: &'a WirModule,
    /// Function byte ranges: (start_offset, end_offset) in the code section.
    pub func_ranges: Vec<(u64, u64)>,
}

pub fn build_dwarf_sections(input: &DwarfInput) -> Vec<(String, Vec<u8>)>
```

Returns a list of `(section_name, bytes)` pairs to be added as custom sections.

### Integration Point: `wado-compiler/src/codegen/emit.rs`

```
pub fn emit_core_module(wir: &WirModule, strip_names: bool, emit_dwarf: bool) -> Vec<u8>
```

After building the core module bytes:

1. If `emit_dwarf`: parse code section to get `func_ranges`
2. Call `build_dwarf_sections`
3. Append each as `wasm_encoder::RawSection`

### Control: `wado-compiler/src/project.rs`

Add `emit_dwarf: bool` to `CompileOptions` (or equivalent). Default: `true` for `-O0`/`-O1`,
`false` for `-O2`/`-O3`/`-Os`.

## `wasm32-unknown-unknown` Compatibility

`gimli::write` is `no_std`-compatible with the `alloc` feature.
Disable the `std` default feature when declaring the dependency:

```toml
[dependencies]
gimli = { version = "0.32", default-features = false, features = ["write"] }
```

This satisfies the requirement that `wado-compiler` compiles for `wasm32-unknown-unknown`.

## Tool Support Matrix

| Tool                                               | Baseline (Name Section only) | After Phase 1 (function DWARF) | After Phase 2 (line table)  |
| -------------------------------------------------- | ---------------------------- | ------------------------------ | --------------------------- |
| `wado run --profile guest` (Firefox Profiler)      | ✅ Function names resolved   | ✅ Same                        | ✅ Per-line attribution     |
| `wado run --profile perfmap` (perf map)            | ✅ Function names resolved   | ✅ Same                        | ✅ Same                     |
| `wado run --profile jitdump` (Linux perf)          | ✅ Function names            | ✅ Same                        | ✅ Source lines visible     |
| Wasmtime `--wasm-backtrace-details` (panic traces) | ❌ Names only, no file:line  | ✅ `src/foo.wado:15` per frame | ✅ Exact statement line     |
| LLDB / GDB function breakpoints                    | ❌ Not supported             | ✅ Function-level breakpoints  | ✅ Statement-level stepping |

## Known Limitations

- **Wasm GC types**: GC-managed structs and variants cannot be described with standard DWARF
  type DIEs. Variable inspection in debuggers will not work for Wado types.
- **Optimized code**: at `-O2`/`-O3`, inlining and reordering make line tables inaccurate.
  DWARF emission is disabled at these levels by default.
- **Component Model wrapping**: DWARF describes the core module. Profilers that instrument
  at the component level may not see the core module's symbols directly.
- **Instruction-level accuracy**: Phase 1 maps entire functions to a single source line.
  Detailed per-statement mapping requires Phase 2.

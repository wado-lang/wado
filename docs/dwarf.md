# DWARF Metadata for Wado

This document describes the plan for adding DWARF debug metadata to the Wado compiler output,
enabling source-level location mapping (file paths, line numbers) for Wado programs.

## Background

Wado already emits a Wasm **Name Section** (`WirNames` / `emit_name_section` in `codegen/emit.rs`),
which provides human-readable function names such as `Point::sum` or `Point^Display::fmt`.

**Verification**: Running `wado run --profile guest benchmark/fts/fts.wado` and inspecting the
resulting Firefox Profiler JSON confirms that symbol names are already fully resolved — all
16 functions appear with names like `f64::fmt_into`, `run`, `write_digits_at`, etc.
No `<unknown>` entries appear in the sampled stacks.

So the Name Section is sufficient for **symbol name resolution** in profilers.
What is missing is **source-level mapping**: file paths and line numbers per stack frame.

DWARF adds the following on top of the Name Section:

- Source file and line number per stack frame
- Per-instruction source location mapping (for fine-grained profiling)
- Function boundary information

## Current State of Debug Information

### Already Available

| Information | Source in WIR |
|---|---|
| Function name | `WirFunction.name.display` (e.g., `Point::sum`) |
| Source file path | `WirFunction.meta.module_source` (`ModuleSource`) |
| Function start line/column | `WirFunction.meta.span` (`Span { line, column, start, end, end_line }`) |
| `gimli` crate | Already in `Cargo.lock` v0.32.3 (transitive via wasmtime) |

### Not Yet Available

| Information | Why Needed | Gap |
|---|---|---|
| Per-instruction byte offsets | `.debug_line` line number program | `wasm-encoder` does not expose current offset during emission |
| Per-statement source spans | Instruction-level source mapping | WIR instructions do not carry `Span`; only functions do |

## DWARF in Wasm

DWARF is embedded as custom sections in the Wasm binary:

| Section | Role |
|---|---|
| `.debug_abbrev` | Abbreviation table for DIE encoding |
| `.debug_str` | Interned string table (file names, function names) |
| `.debug_info` | Compilation unit and subprogram (function) DIEs |
| `.debug_line` | Line number program: instruction byte offset → source line |

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

## Profiler Integration

| Profiler / Tool | Requires | Status after Phase 1 |
|---|---|---|
| `wado run --profile guest` (Firefox Profiler) | Name Section | Already works (Name Section present) |
| `wado run --profile jitdump` (Linux perf) | Name Section + DWARF | Source lines visible |
| `wado run --profile perfmap` (perf map) | Name Section | Already works |
| Wasmtime `--wasm-backtrace-details` | DWARF `.debug_info` | Source file:line in panics |
| LLDB / GDB Wasm DWARF support | DWARF | Function-level stepping |

## Known Limitations

- **Wasm GC types**: GC-managed structs and variants cannot be described with standard DWARF
  type DIEs. Variable inspection in debuggers will not work for Wado types.
- **Optimized code**: at `-O2`/`-O3`, inlining and reordering make line tables inaccurate.
  DWARF emission is disabled at these levels by default.
- **Component Model wrapping**: DWARF describes the core module. Profilers that instrument
  at the component level may not see the core module's symbols directly.
- **Instruction-level accuracy**: Phase 1 maps entire functions to a single source line.
  Detailed per-statement mapping requires Phase 2.

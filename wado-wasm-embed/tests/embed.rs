use std::assert_matches;
use wado_wasm_embed::{Embed, Error, embed};

fn prune(source: &str, keep: &[&str]) -> Vec<u8> {
    prune_with(source, keep, false)
}

fn prune_with(source: &str, keep: &[&str], strip: bool) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("fixture must parse");
    let out = embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|name| keep.contains(&name),
            strip_custom_sections: strip,
        },
    )
    .expect("embed");
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&out)
        .unwrap_or_else(|e| panic!("pruned module must validate: {e}"));
    out
}

fn section_ids(wasm: &[u8]) -> Vec<u8> {
    wasmparser::Parser::new(0)
        .parse_all(wasm)
        .filter_map(|p| p.ok().and_then(|p| p.as_section().map(|(id, _)| id)))
        .collect()
}

fn exports(wasm: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::ExportSection(reader)) = payload {
            for export in reader.into_iter().flatten() {
                names.push(export.name.to_string());
            }
        }
    }
    names
}

fn function_count(wasm: &[u8]) -> usize {
    let mut count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CodeSectionEntry(_)) = payload {
            count += 1;
        }
    }
    count
}

fn function_names(wasm: &[u8]) -> Vec<(u32, String)> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CustomSection(reader)) = payload
            && reader.name() == "name"
        {
            let section = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(
                reader.data(),
                reader.data_offset(),
            ));
            for name in section.into_iter().flatten() {
                if let wasmparser::Name::Function(map) = name {
                    for naming in map.into_iter().flatten() {
                        names.push((naming.index, naming.name.to_string()));
                    }
                }
            }
        }
    }
    names
}

fn memory_import(wasm: &[u8]) -> Option<wasmparser::MemoryType> {
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            for import in reader.into_imports().flatten() {
                if let wasmparser::TypeRef::Memory(mem) = import.ty {
                    return Some(mem);
                }
            }
        }
    }
    None
}

fn count_memory_imports(wasm: &[u8]) -> usize {
    let mut count = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            for import in reader.into_imports().flatten() {
                if matches!(import.ty, wasmparser::TypeRef::Memory(_)) {
                    count += 1;
                }
            }
        }
    }
    count
}

const CALL_CHAIN: &str = r#"
    (module
      (memory 1)
      (func $leaf (param i32) (result i32) (i32.mul (local.get 0) (i32.const 2)))
      (func $kept (export "kept") (param i32) (result i32) (call $leaf (local.get 0)))
      (func $dead (export "dead") (result i32) (i32.const 7))
      (func $dead_leaf (result i32) (call $dead)))
"#;

#[test]
fn keeps_what_a_kept_export_calls() {
    let pruned = prune(CALL_CHAIN, &["kept"]);
    assert_eq!(exports(&pruned), ["kept"]);
    assert_eq!(function_count(&pruned), 2, "kept and leaf");
}

#[test]
fn drops_everything_when_no_export_is_kept() {
    let pruned = prune(CALL_CHAIN, &[]);
    assert!(exports(&pruned).is_empty());
    assert_eq!(function_count(&pruned), 0);
    assert!(memory_import(&pruned).is_some(), "the memory import stays");
}

#[test]
fn a_defined_memory_becomes_an_import() {
    let pruned = prune(
        r#"(module (memory (export "memory") 2 4) (func (export "f")))"#,
        &["f"],
    );
    let memory = memory_import(&pruned).expect("memory import");
    assert_eq!(memory.initial, 2, "the minimum the asset needs is kept");
    assert_eq!(
        memory.maximum, None,
        "the ceiling belongs to the memory the asset no longer has"
    );
    assert!(!section_ids(&pruned).contains(&5), "the definition is gone");
    assert_eq!(exports(&pruned), ["f"], "the memory export is dropped");
}

/// An explicit 64 KiB page is the host's own shape spelled out; the import
/// leaves it unsaid so it matches a host memory that does too.
#[test]
fn an_explicit_default_page_size_is_normalised_away() {
    let pruned = prune(
        r#"(module (memory 1 (pagesize 65536)) (func (export "f")))"#,
        &["f"],
    );
    let memory = memory_import(&pruned).expect("memory import");
    assert_eq!(memory.page_size_log2, None);
}

/// The same normalisation applies to an asset already written against the
/// host's memory: a maximum it declares is a ceiling the host need not honour.
#[test]
fn an_existing_memory_imports_ceiling_is_dropped() {
    let source = r#"
        (module
          (import "env" "memory" (memory 3 8))
          (func (export "load") (result i32) (i32.load (i32.const 0))))
    "#;
    let pruned = prune(source, &["load"]);
    let memory = memory_import(&pruned).expect("memory import");
    assert_eq!(memory.initial, 3);
    assert_eq!(memory.maximum, None);
}

#[test]
fn an_asset_without_memory_still_gets_one() {
    let pruned = prune(r#"(module (func (export "f")))"#, &["f"]);
    assert!(memory_import(&pruned).is_some());
}

#[test]
fn an_existing_memory_import_is_left_alone() {
    let source = r#"
        (module
          (import "env" "memory" (memory 1))
          (func (export "load") (result i32) (i32.load (i32.const 0))))
    "#;
    let pruned = prune(source, &["load"]);
    assert_eq!(count_memory_imports(&pruned), 1);
}

#[test]
fn two_memories_are_rejected() {
    let wasm = wat::parse_str(r#"(module (import "env" "memory" (memory 1)) (memory 1))"#)
        .expect("fixture must parse");
    let err = embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
            strip_custom_sections: false,
        },
    )
    .expect_err("two memories cannot share one component memory");
    assert_matches!(err, Error::Unsupported("more than one memory"));
}

#[test]
fn a_start_section_is_rejected() {
    let wasm = wat::parse_str(r#"(module (func $init) (start $init))"#).expect("fixture");
    let err = embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
            strip_custom_sections: false,
        },
    )
    .expect_err("a start section cannot run inside the embedding");
    assert_matches!(err, Error::Unsupported("start section"));
}

/// `call_indirect` can reach anything the table holds, so an active segment
/// keeps its functions alive even though nothing calls them by index.
#[test]
fn indirect_call_targets_survive() {
    let source = r#"
        (module
          (memory 1)
          (type $unary (func (param i32) (result i32)))
          (table 2 funcref)
          (elem (i32.const 0) $a $b)
          (func $a (type $unary) (local.get 0))
          (func $b (type $unary) (i32.mul (local.get 0) (i32.const 3)))
          (func $unused (type $unary) (i32.const 0))
          (func (export "dispatch") (param i32 i32) (result i32)
            (call_indirect (type $unary) (local.get 0) (local.get 1))))
    "#;
    let pruned = prune(source, &["dispatch"]);
    assert_eq!(function_count(&pruned), 3, "dispatch, a and b");
    assert!(section_ids(&pruned).contains(&4), "table section survives");
    assert!(
        section_ids(&pruned).contains(&9),
        "element section survives"
    );
}

#[test]
fn an_untouched_table_goes_with_its_segments() {
    let source = r#"
        (module
          (memory 1)
          (table 1 funcref)
          (elem (i32.const 0) $only)
          (func $only (result i32) (i32.const 1))
          (func (export "f") (result i32) (i32.const 2)))
    "#;
    let pruned = prune(source, &["f"]);
    assert_eq!(function_count(&pruned), 1);
    let ids = section_ids(&pruned);
    assert!(!ids.contains(&4), "table section is dropped");
    assert!(!ids.contains(&9), "element section is dropped");
}

#[test]
fn globals_follow_the_code_and_the_data_offsets() {
    let source = r#"
        (module
          (memory 1)
          (global $used (mut i32) (i32.const 1))
          (global $offset i32 (i32.const 8))
          (global $dead (mut i32) (i32.const 2))
          (data (global.get $offset) "hi")
          (func (export "f") (result i32) (global.get $used)))
    "#;
    let pruned = prune(source, &["f"]);
    let mut globals = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::GlobalSection(reader)) = payload {
            globals += reader.count();
        }
    }
    assert_eq!(globals, 2, "the used global and the data segment's offset");
}

#[test]
fn passive_data_is_kept_only_where_it_is_initialised_from() {
    let source = r#"
        (module
          (memory 1)
          (data $live "live")
          (data $dead "dead")
          (func (export "f")
            (memory.init $live (i32.const 0) (i32.const 0) (i32.const 4))))
    "#;
    let pruned = prune(source, &["f"]);
    let mut datas = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            datas += reader.count();
        }
    }
    assert_eq!(datas, 1);
}

/// Active segments initialise the shared memory, so they stay whatever the
/// program still reads.
#[test]
fn active_data_survives_an_empty_keep_set() {
    let pruned = prune(
        r#"(module (memory 1) (data (i32.const 0) "hi") (func (export "f")))"#,
        &[],
    );
    let mut datas = 0;
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            datas += reader.count();
        }
    }
    assert_eq!(datas, 1);
}

#[test]
fn function_names_are_filtered_and_renumbered() {
    let source = r#"
        (module
          (memory 1)
          (func $dead (result i32) (i32.const 0))
          (func $live (export "live") (result i32) (i32.const 1)))
    "#;
    let pruned = prune(source, &["live"]);
    assert_eq!(function_names(&pruned), [(0, "live".to_string())]);
}

/// A declarative segment exists so `ref.func` validates; it is filtered to the
/// functions that survived.
#[test]
fn declared_segments_shrink_to_the_surviving_functions() {
    let source = r#"
        (module
          (memory 1)
          (elem declare func $referenced $dead)
          (func $referenced (result i32) (i32.const 1))
          (func $dead (result i32) (i32.const 2))
          (func (export "f") (result funcref) (ref.func $referenced)))
    "#;
    let pruned = prune(source, &["f"]);
    assert_eq!(function_count(&pruned), 2, "f and referenced");
}

fn custom_sections(wasm: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CustomSection(reader)) = payload {
            names.push(reader.name().to_string());
        }
    }
    names
}

const WITH_CUSTOM_SECTIONS: &str = r#"
    (module
      (@custom "producers" "\00")
      (@custom "target_features" "\00")
      (@custom ".debug_info" "stale")
      (@custom "metadata.code.branch_hint" "stale")
      (@custom "something.unknown" "stale")
      (memory 1)
      (func $dead (result i32) (i32.const 0))
      (func $live (export "live") (result i32) (i32.const 1)))
"#;

/// The prune renumbers everything, so only the sections that name no index
/// survive it — plus the `name` section, which is rebuilt.
#[test]
fn only_the_custom_sections_that_stay_true_survive() {
    let pruned = prune(WITH_CUSTOM_SECTIONS, &["live"]);
    let mut sections = custom_sections(&pruned);
    sections.sort();
    assert_eq!(sections, ["name", "producers", "target_features"]);
}

#[test]
fn stripping_drops_every_custom_section() {
    let pruned = prune_with(WITH_CUSTOM_SECTIONS, &["live"], true);
    assert!(custom_sections(&pruned).is_empty());
}

/// `ref.func` needs its target declared, and an export is a declaration. When
/// the prune drops that export the function is still live, so the declaration
/// has to be put back or the module no longer validates.
#[test]
fn a_ref_func_target_that_loses_its_declaring_export_is_redeclared() {
    let source = r#"
        (module
          (memory 1)
          (func $helper (export "helper") (result i32) (i32.const 1))
          (func (export "setup") (result funcref) (ref.func $helper)))
    "#;
    let pruned = prune(source, &["setup"]);
    assert_eq!(
        function_count(&pruned),
        2,
        "helper stays, ref.func names it"
    );
}

/// Same loss through an active segment: the table it filled went dead with it.
#[test]
fn a_ref_func_target_that_loses_its_declaring_segment_is_redeclared() {
    let source = r#"
        (module
          (memory 1)
          (table 1 funcref)
          (elem (i32.const 0) $helper)
          (func $helper (result i32) (i32.const 1))
          (func (export "setup") (result funcref) (ref.func $helper)))
    "#;
    let pruned = prune(source, &["setup"]);
    assert_eq!(function_count(&pruned), 2);
}

/// An imported table is kept whole, so the segments that fill it are still
/// live — the same rule active data segments follow.
#[test]
fn active_segments_of_an_imported_table_survive() {
    let source = r#"
        (module
          (import "env" "table" (table 1 funcref))
          (memory 1)
          (type $unary (func (result i32)))
          (elem (i32.const 0) $filled)
          (func $filled (type $unary) (i32.const 1))
          (func (export "f") (result i32) (i32.const 2)))
    "#;
    let pruned = prune(source, &["f"]);
    assert_eq!(function_count(&pruned), 2, "f and the segment's function");
}

/// The `name` section is metadata a validator never looks at, so a broken one
/// costs the asset its names, not the build.
#[test]
fn a_broken_name_section_is_dropped_rather_than_failing() {
    let source = r#"
        (module
          (@custom "name" "\ff\ff\ff\ff")
          (memory 1)
          (func (export "f") (result i32) (i32.const 1)))
    "#;
    let pruned = prune(source, &["f"]);
    assert!(!custom_sections(&pruned).contains(&"name".to_string()));
}

/// The bytes of an active segment, as `(address, contents)` per emitted piece.
fn data_segments(wasm: &[u8]) -> Vec<(i32, Vec<u8>)> {
    let mut segments = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            for data in reader.into_iter().flatten() {
                let wasmparser::DataKind::Active { offset_expr, .. } = data.kind else {
                    continue;
                };
                let address = match offset_expr.get_operators_reader().read() {
                    Ok(wasmparser::Operator::I32Const { value }) => value,
                    other => panic!("expected a constant address, got {other:?}"),
                };
                segments.push((address, data.data.to_vec()));
            }
        }
    }
    segments
}

/// A module whose `.rodata` equivalent is one 16-byte segment at address 8,
/// read four bytes at a time by four functions.
fn quarters(dataref: Option<&str>) -> String {
    let section = match dataref {
        Some(text) => format!("(@custom \"wado.dataref\" (after data) {text:?})"),
        None => String::new(),
    };
    format!(
        r#"
        (module
          (memory 1)
          (data (i32.const 8) "AAAABBBBCCCCDDDD")
          (func $a (export "a") (result i32) (i32.load (i32.const 8)))
          (func $b (export "b") (result i32) (i32.load (i32.const 12)))
          (func $c (export "c") (result i32) (i32.load (i32.const 16)))
          (func $d (export "d") (result i32) (i32.load (i32.const 20)))
          {section})
    "#
    )
}

const QUARTERS_MAP: &str = "a 0:0+4\nb 0:4+4\nc 0:8+4\nd 0:12+4\n";

#[test]
fn a_data_reference_map_prunes_an_active_segment_by_the_byte() {
    let pruned = prune(&quarters(Some(QUARTERS_MAP)), &["b"]);
    assert_eq!(data_segments(&pruned), [(12, b"BBBB".to_vec())]);
}

#[test]
fn without_a_map_an_active_segment_stays_whole() {
    let pruned = prune(&quarters(None), &["b"]);
    assert_eq!(data_segments(&pruned), [(8, b"AAAABBBBCCCCDDDD".to_vec())]);
}

/// A second segment costs more header than a short gap costs payload, so
/// ranges close enough together are kept as one.
#[test]
fn ranges_separated_by_less_than_a_segment_header_are_merged() {
    let pruned = prune(&quarters(Some(QUARTERS_MAP)), &["a", "c"]);
    assert_eq!(data_segments(&pruned), [(8, b"AAAABBBBCCCC".to_vec())]);
}

#[test]
fn a_map_that_reaches_nothing_drops_the_segment_and_its_section() {
    let pruned = prune(&quarters(Some(QUARTERS_MAP)), &[]);
    assert_eq!(data_segments(&pruned), []);
    assert!(!section_ids(&pruned).contains(&11), "no data section");
}

/// The map describes the segments before the split, so keeping it would leave
/// the asset carrying offsets into a segment that no longer exists.
#[test]
fn the_map_itself_never_survives_the_prune() {
    let pruned = prune(&quarters(Some(QUARTERS_MAP)), &["a"]);
    assert!(!custom_sections(&pruned).contains(&"wado.dataref".to_string()));
}

#[test]
fn a_map_naming_an_unknown_function_is_rejected() {
    let source = quarters(Some("a 0:0+4\nnobody 0:4+4\n"));
    let wasm = wat::parse_str(&source).expect("fixture must parse");
    let error = embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
            strip_custom_sections: false,
        },
    )
    .expect_err("a map that has drifted from the module must not be used");
    assert_matches!(error, Error::DataRef(_));
}

#[test]
fn a_map_reaching_past_the_end_of_its_segment_is_rejected() {
    let source = quarters(Some("a 0:0+64\n"));
    let wasm = wat::parse_str(&source).expect("fixture must parse");
    let error = embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
            strip_custom_sections: false,
        },
    )
    .expect_err("a range outside its segment must not be used");
    assert_matches!(error, Error::DataRef(_));
}

/// `memory.init` names a segment by index, so a passive segment is never split
/// and the indices of the ones that remain still have to line up.
#[test]
fn passive_segments_keep_their_indices_across_a_split() {
    let source = r#"
        (module
          (memory 1)
          (data (i32.const 8) "AAAABBBB")
          (data $passive "passive")
          (func $a (export "a") (result i32) (i32.load (i32.const 8)))
          (func (export "init")
            (memory.init $passive (i32.const 64) (i32.const 0) (i32.const 7)))
          (@custom "wado.dataref" (after data) "a 0:0+4\n"))
    "#;
    let pruned = prune(source, &["a", "init"]);
    assert_eq!(data_segments(&pruned), [(8, b"AAAA".to_vec())]);
    let mut init_targets = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::CodeSectionEntry(body)) = payload {
            let mut ops = body.get_operators_reader().expect("body must read");
            while !ops.eof() {
                if let Ok(wasmparser::Operator::MemoryInit { data_index, .. }) = ops.read() {
                    init_targets.push(data_index);
                }
            }
        }
    }
    assert_eq!(init_targets, [1], "the passive segment is still the second");
}

/// A segment whose address is not a constant cannot have its pieces address
/// themselves, so it is kept whole however narrow the map is.
#[test]
fn a_segment_at_a_computed_address_is_never_split() {
    let source = r#"
        (module
          (memory 1)
          (global $base i32 (i32.const 8))
          (data (global.get $base) "AAAABBBB")
          (func $a (export "a") (result i32) (i32.load (i32.const 8)))
          (@custom "wado.dataref" (after data) "a 0:0+4\n"))
    "#;
    let pruned = prune(source, &["a"]);
    let mut kept = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            for data in reader.into_iter().flatten() {
                kept.push(data.data.to_vec());
            }
        }
    }
    assert_eq!(kept, [b"AAAABBBB".to_vec()]);
}

/// A passive segment is named by index and copied by `memory.init`, so no map
/// can narrow it — and a map that names one must not be honoured halfway.
#[test]
fn a_map_naming_a_passive_segment_leaves_it_to_memory_init() {
    let source = r#"
        (module
          (memory 1)
          (data $passive "passive")
          (func $a (export "a")
            (memory.init $passive (i32.const 0) (i32.const 0) (i32.const 7)))
          (@custom "wado.dataref" (after data) "a 0:0+4\n"))
    "#;
    let pruned = prune(source, &["a"]);
    let mut kept = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&pruned) {
        if let Ok(wasmparser::Payload::DataSection(reader)) = payload {
            for data in reader.into_iter().flatten() {
                kept.push(data.data.to_vec());
            }
        }
    }
    assert_eq!(
        kept,
        [b"passive".to_vec()],
        "the whole segment, not 4 bytes"
    );
}

/// The same segment, with nothing initialising memory from it: the map must not
/// be what keeps it alive either.
#[test]
fn a_map_naming_a_passive_segment_does_not_keep_it_alive() {
    let source = r#"
        (module
          (memory 1)
          (data $passive "passive")
          (func $a (export "a") (result i32) (i32.const 1))
          (@custom "wado.dataref" (after data) "a 0:0+4\n"))
    "#;
    assert_eq!(data_segments(&prune(source, &["a"])), []);
    assert!(
        !section_ids(&prune(source, &["a"])).contains(&11),
        "no data section"
    );
}

fn dataref_error(map: &str) -> Error {
    let wasm = wat::parse_str(&quarters(Some(map))).expect("fixture must parse");
    embed(
        &wasm,
        &Embed {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
            strip_custom_sections: false,
        },
    )
    .expect_err("a map that cannot be trusted must not be used")
}

/// A map that names nothing reads exactly like one that failed to resolve, and
/// honouring it would prune every data segment away.
#[test]
fn an_empty_map_is_rejected_rather_than_pruning_everything() {
    assert_matches!(dataref_error(""), Error::DataRef(_));
    assert_matches!(dataref_error("\n  \n"), Error::DataRef(_));
}

/// `offset + size` past the end of the address space wraps in release, so the
/// range has to be refused where it is built, not where it is sliced.
#[test]
fn a_range_that_cannot_end_is_rejected() {
    assert_matches!(dataref_error("a 0:4294967295+8\n"), Error::DataRef(_));
    assert_matches!(dataref_error("a 0:1+4294967295\n"), Error::DataRef(_));
}

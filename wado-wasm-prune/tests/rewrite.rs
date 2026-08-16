use wado_wasm_prune::{Error, Rewrite, rewrite};

fn prune(source: &str, keep: &[&str]) -> Vec<u8> {
    let wasm = wat::parse_str(source).expect("fixture must parse");
    let out = rewrite(
        &wasm,
        &Rewrite {
            memory_import: ("env", "memory"),
            keep_export: &|name| keep.contains(&name),
        },
    )
    .expect("rewrite");
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
    assert_eq!(memory.initial, 2);
    assert_eq!(memory.maximum, Some(4), "the shape survives the rewrite");
    assert!(!section_ids(&pruned).contains(&5), "the definition is gone",);
    assert_eq!(exports(&pruned), ["f"], "the memory export is dropped");
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
    let err = rewrite(
        &wasm,
        &Rewrite {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
        },
    )
    .expect_err("two memories cannot share one component memory");
    assert!(matches!(err, Error::Unsupported("more than one memory")));
}

#[test]
fn a_start_section_is_rejected() {
    let wasm = wat::parse_str(r#"(module (func $init) (start $init))"#).expect("fixture");
    let err = rewrite(
        &wasm,
        &Rewrite {
            memory_import: ("env", "memory"),
            keep_export: &|_| true,
        },
    )
    .expect_err("a start section cannot run inside the embedding");
    assert!(matches!(err, Error::Unsupported("start section")));
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

//! End-to-end tests for the optional `probe` export.
//!
//! A probe reports how many leading bytes of an input determine the
//! generator's output. Two consequences are pinned here:
//!
//! 1. `generate` sees the input cut to that extent, so a generator that reads
//!    past what it declared gets EOF rather than bytes the cache key does not
//!    cover.
//! 2. A later build hashes only that extent, so editing the payload behind the
//!    header is a cache hit and editing the header is a miss.

use std::fs;

use crate::common::wado_in;
use predicates::prelude::*;

/// The extent is everything through the first newline. `generate` then sees
/// only that first line, which is what makes the clamp observable: the payload
/// below it never reaches the generated source.
const HEADER_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn probe(path: String, content: Stream<u8>) -> Result<u64, Error> {
    let mut n: u64 = 0;
    loop {
        let chunk = content.read(1);
        if chunk.items.len() == 0 {
            break;
        }
        n += 1;
        if chunk.items[0] == b'\n' {
            break;
        }
        if chunk.result != CopyResult::Completed {
            break;
        }
    }
    content.drop();
    return Result::Ok(n);
}

export fn generate(req: Request) -> Result<Response, Error> {
    let head = read_text(req.primary.content).unwrap();
    return Result::Ok(Response {
        files: [OutputFile {
            path: "out.wado",
            content: `pub fn hello() -> i32 { return ${head.trim()}; }`,
            is_entry: true,
        }],
    });
}
"#;

fn write_project(root: &std::path::Path, schema: &str) {
    fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"probed\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/gen.wado"), HEADER_GENERATOR).unwrap();
    fs::write(root.join("src/schema.bin"), schema).unwrap();
    fs::write(
        root.join("src/main.wado"),
        r#"use { println, Stdout } from "core:cli";
use { hello } from "./schema.bin"
    with { generator: { module: "./gen.wado" } };

export fn run() with Stdout {
    println(`${hello()}`);
}
"#,
    )
    .unwrap();
}

/// The payload past the extent never reaches `generate`: the output is built
/// from the header alone.
#[test]
fn generate_sees_the_input_cut_to_its_extent() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root, "7\nPAYLOAD THAT MUST NOT REACH THE GENERATOR\n");

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("7"));
}

/// Editing behind the extent is a cache hit, editing the header is a miss.
/// This is the whole point of the probe: a checkpoint's payload can change
/// without the generator running again.
#[test]
fn only_the_extent_decides_whether_the_generator_reruns() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write_project(root, "7\nfirst payload\n");

    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("7"));

    let metadata_before = recorded_metadata(root);
    let extent = metadata_before["primary"]["extent"].as_u64();
    assert_eq!(extent, Some(2), "extent covers `7\\n` and nothing further");

    // Rewrite the payload only. The recorded hash covers the header alone, so
    // it still matches and nothing is regenerated.
    fs::write(
        root.join("src/schema.bin"),
        "7\na completely different payload\n",
    )
    .unwrap();
    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("7"));
    assert_eq!(
        recorded_metadata(root)["primary"]["hash"],
        metadata_before["primary"]["hash"],
        "a payload-only edit must not change the recorded identity"
    );

    // Rewrite the header. The extent's own bytes changed, so the build misses
    // and the generated source follows.
    fs::write(
        root.join("src/schema.bin"),
        "9\na completely different payload\n",
    )
    .unwrap();
    wado_in(root)
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout(predicate::str::contains("9"));
}

fn recorded_metadata(root: &std::path::Path) -> serde_json::Value {
    let dir = root.join("build/kiln");
    let record = walk_for_suffix(&dir, ".kiln.json").expect("a recorded invocation");
    serde_json::from_str(&fs::read_to_string(record).unwrap()).unwrap()
}

fn walk_for_suffix(dir: &std::path::Path, suffix: &str) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(dir).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(hit) = walk_for_suffix(&path, suffix) {
                return Some(hit);
            }
        } else if path.to_string_lossy().ends_with(suffix) {
            return Some(path);
        }
    }
    None
}

//! Remote modules and Kiln inputs, fetched from a loopback server: what
//! `wado.lock` records, what a pin holds a read to, and how little of a Kiln
//! input crosses the network.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use predicates::prelude::*;
use wado_compiler::remote::Integrity;

use crate::common::wado_in;

/// One request the server answered: its path and its `Range` header.
#[derive(Debug, Clone)]
struct Served {
    path: String,
    range: Option<String>,
}

/// An HTTP/1.1 server on `127.0.0.1` answering each path from `files`,
/// honoring `Range: bytes=a-b`, and recording every request.
struct Server {
    port: u16,
    files: Arc<Mutex<IndexMap<String, Vec<u8>>>>,
    log: Arc<Mutex<Vec<Served>>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = Arc::new(Mutex::new(IndexMap::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (served, logged) = (Arc::clone(&files), Arc::clone(&log));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                answer(stream, &served, &logged);
            }
        });
        Self { port, files, log }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{path}", self.port)
    }

    fn serve(&self, path: &str, bytes: &[u8]) {
        self.files
            .lock()
            .unwrap()
            .insert(format!("/{path}"), bytes.to_vec());
    }

    fn withdraw(&self, path: &str) {
        self.files.lock().unwrap().shift_remove(&format!("/{path}"));
    }

    fn requests(&self) -> Vec<Served> {
        self.log.lock().unwrap().clone()
    }
}

fn answer(stream: TcpStream, files: &Mutex<IndexMap<String, Vec<u8>>>, log: &Mutex<Vec<Served>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let mut range = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("range")
        {
            range = Some(value.trim().to_string());
        }
    }
    log.lock().unwrap().push(Served {
        path: path.clone(),
        range: range.clone(),
    });
    let body = files.lock().unwrap().get(&path).cloned();
    let mut stream = stream;
    let Some(body) = body else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        return;
    };
    let total = body.len();
    let (status, part, content_range) = match range
        .as_deref()
        .and_then(|r| r.strip_prefix("bytes="))
        .and_then(|r| r.split_once('-'))
    {
        Some((start, end)) => {
            let start: usize = start.parse().unwrap();
            let end = end.parse::<usize>().unwrap().min(total - 1);
            (
                "206 Partial Content",
                &body[start..=end],
                Some(format!("bytes {start}-{end}/{total}")),
            )
        }
        None => ("200 OK", &body[..], None),
    };
    let mut head = format!(
        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n",
        part.len()
    );
    if let Some(content_range) = content_range {
        head.push_str(&format!("content-range: {content_range}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(part);
}

const UTIL: &str = "pub fn exclaim(s: String) -> String { return `${s}!`; }\n";

fn write_package(root: &Path, main: &str) {
    std::fs::write(
        root.join("wado.toml"),
        "[package]\nname = \"remote\"\nversion = \"0.1.0\"\n\n[world]\n\"wasi:cli/command\" = \"src/main.wado\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.wado"), main).unwrap();
}

fn main_using(url: &str, with: &str) -> String {
    format!(
        "use {{ println, Stdout }} from \"core:cli\";\n\
         use {{ exclaim }} from \"{url}\"{with};\n\n\
         export fn run() with Stdout {{\n    println(exclaim(\"remote\"));\n}}\n"
    )
}

/// A `wado` whose fetched files land in `wado_root` rather than the user's.
fn wado(dir: &Path, wado_root: &Path) -> assert_cmd::Command {
    let mut cmd = wado_in(dir);
    cmd.env("WADO_ROOT", wado_root);
    cmd
}

/// A package records what its first read of a remote module saw, and holds
/// every later read to it.
#[test]
fn the_lock_pins_a_remote_module_on_its_first_read() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let project = tempfile::tempdir().unwrap();
    let wado_root = tempfile::tempdir().unwrap();
    write_package(project.path(), &main_using(&server.url("util.wado"), ""));

    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("remote!\n");
    let lock = std::fs::read_to_string(project.path().join("wado.lock")).unwrap();
    assert!(
        lock.contains(&format!("url = \"{}\"", server.url("util.wado"))),
        "{lock}"
    );

    // The cache still holds the pinned bytes, so only a cold one fetches what
    // the server serves now.
    server.serve(
        "util.wado",
        b"pub fn exclaim(s: String) -> String { return s; }\n",
    );
    wado(project.path(), wado_root.path())
        .args(["run", "--no-cache", "src/main.wado"])
        .assert()
        .success()
        .stdout("remote!\n");
    let cold_root = tempfile::tempdir().unwrap();
    wado(project.path(), cold_root.path())
        .args(["run", "--no-cache", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "does not match its integrity: `wado.lock` pins",
        ));
}

/// A file in no package has no lock to record in, so its `use` must pin the
/// module, and the error says what to write.
#[test]
fn a_file_in_no_package_must_pin_a_remote_module() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let wado_root = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.wado"),
        main_using(&server.url("util.wado"), ""),
    )
    .unwrap();

    wado(dir.path(), wado_root.path())
        .args(["run", "main.wado"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("is not pinned")
                .and(predicate::str::contains("integrity: \"sha256:")),
        );
}

/// A pinned read is kept by content, so it needs no server once fetched.
#[test]
fn a_pinned_module_is_read_from_the_cache_once_fetched() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let wado_root = tempfile::tempdir().unwrap();
    let pin = Integrity::of(UTIL.as_bytes());
    std::fs::write(
        dir.path().join("main.wado"),
        main_using(
            &server.url("util.wado"),
            &format!(" with {{ integrity: \"{pin}\" }}"),
        ),
    )
    .unwrap();

    wado(dir.path(), wado_root.path())
        .args(["run", "main.wado"])
        .assert()
        .success()
        .stdout("remote!\n");
    server.withdraw("util.wado");
    wado(dir.path(), wado_root.path())
        .args(["run", "--no-cache", "main.wado"])
        .assert()
        .success()
        .stdout("remote!\n");
}

const HEADER_GENERATOR: &str = r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn probe(path: String, mut content: Stream<u8>) -> Result<u64, Error> {
    let mut n: u64 = 0;
    loop {
        let [end, chunk] = content.start_read(1).finish();
        content = end;
        if chunk.items.len() > 0 {
            n += 1;
            if chunk.items[0] == b'\n' {
                break;
            }
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

/// A Kiln input read over HTTP crosses the network only as far as the probe
/// reads, and the lock pins that part.
#[test]
fn a_remote_kiln_input_is_fetched_by_range_and_pinned_to_its_extent() {
    let server = Server::start();
    let mut model = b"7\n".to_vec();
    model.resize(4 << 20, b'x');
    server.serve("model.bin", &model);
    let project = tempfile::tempdir().unwrap();
    let wado_root = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        &format!(
            "use {{ println, Stdout }} from \"core:cli\";\n\
             use {{ hello }} from \"{}\"\n    with {{ generator: {{ module: \"./gen.wado\" }} }};\n\n\
             export fn run() with Stdout {{\n    println(`${{hello()}}`);\n}}\n",
            server.url("model.bin")
        ),
    );
    std::fs::write(project.path().join("src/gen.wado"), HEADER_GENERATOR).unwrap();

    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("7\n");

    let requests = server.requests();
    assert!(requests.iter().all(|r| r.path == "/model.bin"));
    assert!(
        !requests.is_empty() && requests.iter().all(|r| r.range.is_some()),
        "every read of the model is a range: {requests:?}"
    );
    let lock = std::fs::read_to_string(project.path().join("wado.lock")).unwrap();
    assert!(lock.contains("extent = 2"), "{lock}");
    assert!(
        lock.contains(&format!("integrity = \"{}\"", Integrity::of(b"7\n"))),
        "{lock}"
    );
}

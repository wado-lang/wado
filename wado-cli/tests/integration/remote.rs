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

/// One request the server answered: its path, its `Range` header, and, once
/// the answer is written or the client hung up, whether all of it went out.
#[derive(Debug, Clone)]
struct Served {
    path: String,
    range: Option<String>,
    sent_whole: Option<bool>,
}

/// Whether a [`Server`] answers `Range: bytes=a-b` with that range.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ranges {
    Honored,
    /// The whole file, as some servers send it.
    Ignored,
}

/// An HTTP/1.1 server on `127.0.0.1` answering each path from `files`, and
/// recording every request.
struct Server {
    port: u16,
    files: Arc<Mutex<IndexMap<String, Answer>>>,
    log: Arc<Mutex<Vec<Served>>>,
}

#[derive(Clone)]
enum Answer {
    File(Vec<u8>),
    RedirectTo(String),
}

impl Server {
    fn start() -> Self {
        Self::start_with(Ranges::Honored)
    }

    fn start_with(ranges: Ranges) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = Arc::new(Mutex::new(IndexMap::new()));
        let log = Arc::new(Mutex::new(Vec::new()));
        let (served, logged) = (Arc::clone(&files), Arc::clone(&log));
        // A connection each, as a client holding one body open may ask again.
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (served, logged) = (Arc::clone(&served), Arc::clone(&logged));
                std::thread::spawn(move || answer(stream, ranges, &served, &logged));
            }
        });
        Self { port, files, log }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}/{path}", self.port)
    }

    fn serve(&self, path: &str, bytes: &[u8]) {
        self.answer(path, Answer::File(bytes.to_vec()));
    }

    fn redirect(&self, path: &str, location: &str) {
        self.answer(path, Answer::RedirectTo(location.to_string()));
    }

    fn answer(&self, path: &str, answer: Answer) {
        self.files
            .lock()
            .unwrap()
            .insert(format!("/{path}"), answer);
    }

    fn withdraw(&self, path: &str) {
        self.files.lock().unwrap().shift_remove(&format!("/{path}"));
    }

    fn requests(&self) -> Vec<Served> {
        self.log.lock().unwrap().clone()
    }

    /// Every request, once each answer is written or abandoned: a client that
    /// exited has hung up, but the writer learns so a moment later.
    fn settled_requests(&self) -> Vec<Served> {
        for _ in 0..1000 {
            let requests = self.requests();
            if requests.iter().all(|r| r.sent_whole.is_some()) {
                return requests;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("an answer never settled: {:?}", self.requests());
    }
}

fn answer(
    stream: TcpStream,
    ranges: Ranges,
    files: &Mutex<IndexMap<String, Answer>>,
    log: &Mutex<Vec<Served>>,
) {
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
    let entry = {
        let mut log = log.lock().unwrap();
        log.push(Served {
            path: path.clone(),
            range: range.clone(),
            sent_whole: None,
        });
        log.len() - 1
    };
    let body = files.lock().unwrap().get(&path).cloned();
    let mut stream = stream;
    let body = match body {
        Some(Answer::File(body)) => body,
        Some(Answer::RedirectTo(location)) => {
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
            return;
        }
        None => {
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            return;
        }
    };
    let total = body.len();
    let (status, part, content_range) = match range
        .as_deref()
        .filter(|_| ranges == Ranges::Honored)
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
    let sent = stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(part));
    log.lock().unwrap()[entry].sent_whole = Some(sent.is_ok());
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
        ))
        .stderr(predicate::str::contains("run `wado update`"));

    // `wado update` drops the pin, so the next build records the new content.
    wado(project.path(), cold_root.path())
        .arg("update")
        .assert()
        .success();
    wado(project.path(), cold_root.path())
        .args(["run", "--no-cache", "src/main.wado"])
        .assert()
        .success()
        .stdout("remote\n");
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

/// A pin written anywhere holds every `use` of the URL, whichever the loader
/// reaches first, a Wasm asset's as a module's.
#[test]
fn a_pin_written_by_a_later_module_pins_an_earlier_use() {
    const SEVEN: &str = "(module (func (export \"seven\") (result i32) i32.const 7))";
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    server.serve("seven.wat", SEVEN.as_bytes());
    let url = server.url("util.wado");
    let wat = server.url("seven.wat");
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: String| std::fs::write(dir.path().join(name), text).unwrap();
    write(
        "main.wado",
        "use { println, Stdout } from \"core:cli\";\n\
         use { a } from \"./a.wado\";\n\
         use { b } from \"./b.wado\";\n\n\
         export fn run() with Stdout {\n    println(`${a()}${b()}`);\n}\n"
            .to_string(),
    );
    write(
        "a.wado",
        format!(
            "use {{ exclaim }} from \"{url}\";\n\
             use {{ seven }} from \"{wat}\" with {{ type: \"wat\" }};\n\
             pub fn a() -> String {{ return exclaim(`a${{seven()}}`); }}\n"
        ),
    );
    write(
        "b.wado",
        "use { c } from \"./c.wado\";\npub fn b() -> String { return c(); }\n".to_string(),
    );
    write(
        "c.wado",
        format!(
            "use {{ exclaim }} from \"{url}\" with {{ integrity: \"{}\" }};\n\
             use {{ seven }} from \"{wat}\" with {{ type: \"wat\", integrity: \"{}\" }};\n\
             pub fn c() -> String {{ return exclaim(`c${{seven()}}`); }}\n",
            Integrity::of(UTIL.as_bytes()),
            Integrity::of(SEVEN.as_bytes())
        ),
    );

    wado(dir.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "main.wado"])
        .assert()
        .success()
        .stdout("a7!c7!\n");
}

/// A redirect is held to the rule a `use` is: `http://` reaches only this
/// machine.
#[test]
fn a_redirect_off_this_machine_over_http_is_refused() {
    let server = Server::start();
    server.redirect("util.wado", "http://example.com/util.wado");
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &main_using(&server.url("util.wado"), ""));

    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("`http://` names only `localhost`"));
}

/// A redirect stays on the side of the boundary between this network and the
/// internet that its first URL is on.
#[test]
fn a_redirect_stays_on_its_side_of_the_internet() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let local = server.url("util.wado").replace("127.0.0.1", "localhost");
    server.redirect("near.wado", &local);
    server.redirect("far.wado", "https://example.com/util.wado");
    let run = |name: &str| {
        let project = tempfile::tempdir().unwrap();
        write_package(project.path(), &main_using(&server.url(name), ""));
        wado(project.path(), tempfile::tempdir().unwrap().path())
            .args(["run", "src/main.wado"])
            .assert()
    };

    run("near.wado").success().stdout("remote!\n");
    run("far.wado").failure().stderr(predicate::str::contains(
        "a redirect from this network never reaches the internet",
    ));
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
    server.serve("model.bin", &model(b"7\n"));
    let project = model_package(&server);
    let wado_root = tempfile::tempdir().unwrap();

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

/// A 4 MiB model whose header, up to its first newline, is `header`.
fn model(header: &[u8]) -> Vec<u8> {
    let mut model = header.to_vec();
    model.resize(4 << 20, b'x');
    model
}

/// A package whose `run` prints what the header of the model `server` serves
/// generates.
fn model_package(server: &Server) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
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
    project
}

/// A probe's extent is decided by the bytes inside it, so a model whose header
/// moved changed, and is held to the extent the lock pins rather than pinned
/// afresh.
#[test]
fn a_moved_extent_is_held_to_the_pin_of_the_old_one() {
    let server = Server::start();
    server.serve("model.bin", &model(b"7\n"));
    let project = model_package(&server);
    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .success();

    server.serve("model.bin", &model(b"10\n"));
    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "--no-cache", "src/main.wado"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "does not match its integrity: `wado.lock` pins",
        ));
    let lock = std::fs::read_to_string(project.path().join("wado.lock")).unwrap();
    assert!(!lock.contains("extent = 3"), "{lock}");
}

/// A probe reading less than the lock pins is held to the bytes kept for that
/// pin rather than to a fresh fetch of them.
#[test]
fn a_shorter_extent_is_held_to_the_bytes_kept_for_the_longer_one() {
    let server = Server::start();
    server.serve("model.bin", &model(b"10\n"));
    let project = model_package(&server);
    let wado_root = tempfile::tempdir().unwrap();
    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("10\n");

    let generator = project.path().join("src/gen.wado");
    let two_bytes = std::fs::read_to_string(&generator)
        .unwrap()
        .replace("chunk.items[0] == b'\\n'", "n == 2");
    std::fs::write(&generator, two_bytes).unwrap();
    let before = server.requests().len();
    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("10\n");
    let requests = server.requests();
    assert!(
        requests[before..]
            .iter()
            .all(|r| r.range.as_deref() != Some("bytes=0-2")),
        "{requests:?}"
    );
}

/// A lock that cannot be read is reported, never replaced by one holding only
/// the new pin.
#[test]
fn an_unreadable_lock_is_an_error_and_kept() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let project = tempfile::tempdir().unwrap();
    write_package(project.path(), &main_using(&server.url("util.wado"), ""));
    let unreadable = b"version = 1\n\xff\n";
    std::fs::write(project.path().join("wado.lock"), unreadable).unwrap();

    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .failure();
    assert_eq!(
        std::fs::read(project.path().join("wado.lock")).unwrap(),
        unreadable
    );
}

/// A generated import inside a remote module names its generator by the
/// consumer's `[build-dependencies]`: no `wado.toml` travels with the module.
#[test]
fn a_remote_module_clause_resolves_against_the_consumer_build_dependencies() {
    let server = Server::start();
    server.serve(
        "lib.wado",
        b"pub use { greeting } from \"./greeting.idl\"\n    \
          with { generator: { module: \"lib:gen\" } };\n",
    );
    server.serve("greeting.idl", b"hi from afar\n");
    let root = tempfile::tempdir().unwrap();
    let generator = root.path().join("gen");
    std::fs::create_dir_all(generator.join("src")).unwrap();
    std::fs::write(
        generator.join("wado.toml"),
        "[package]\nname = \"gen\"\nversion = \"0.1.0\"\n\n\
         [world]\n\"core:kiln/generator\" = \"src/generator.wado\"\n",
    )
    .unwrap();
    std::fs::write(
        generator.join("src/generator.wado"),
        r#"use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

export fn generate(req: Request) -> Result<Response, Error> {
    let text = read_text(req.primary.content).unwrap();
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: `pub fn greeting() -> String { return "${text.trim()}"; }`,
            is_entry: true,
        }],
    });
}
"#,
    )
    .unwrap();
    let project = root.path().join("app");
    std::fs::create_dir(&project).unwrap();
    write_package(
        &project,
        &format!(
            "use {{ println, Stdout }} from \"core:cli\";\n\
             use {{ greeting }} from \"{}\";\n\n\
             export fn run() with Stdout {{\n    println(greeting());\n}}\n",
            server.url("lib.wado")
        ),
    );
    let manifest = std::fs::read_to_string(project.join("wado.toml")).unwrap();
    std::fs::write(
        project.join("wado.toml"),
        format!("{manifest}\n[build-dependencies]\n\"lib:gen\" = {{ path = \"../gen\" }}\n"),
    )
    .unwrap();

    wado(&project, tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("hi from afar\n");
}

/// A package whose `run` prints what its generator, importing `exclaim` from
/// `url` with the import attributes `with`, generates.
fn generator_package(url: &str, with: &str) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    write_package(
        project.path(),
        "use { println, Stdout } from \"core:cli\";\n\
         use { hello } from \"./input.txt\" with { generator: { module: \"./gen.wado\" } };\n\n\
         export fn run() with Stdout {\n    println(hello());\n}\n",
    );
    std::fs::write(project.path().join("src/input.txt"), "").unwrap();
    std::fs::write(
        project.path().join("src/gen.wado"),
        format!(
            r#"use {{ Request, Response, OutputFile, Error }} from "core:kiln";
use {{ exclaim }} from "{url}"{with};

export fn generate(req: Request) -> Result<Response, Error> {{
    return Result::Ok(Response {{
        files: [OutputFile {{
            path: "out.wado",
            content: `pub fn hello() -> String {{ return "${{exclaim("generated")}}"; }}`,
            is_entry: true,
        }}],
    }});
}}
"#
        ),
    )
    .unwrap();
    project
}

/// A pinned remote module is as deterministic as a local one, so a generator
/// imports it as any module does.
#[test]
fn a_generator_imports_a_pinned_remote_module() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let project = generator_package(
        &server.url("util.wado"),
        &format!(
            " with {{ integrity: \"{}\" }}",
            Integrity::of(UTIL.as_bytes())
        ),
    );

    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("generated!\n");
}

/// A generator built on what the lock pins is rebuilt once `wado update` drops
/// the pin, though none of its own sources changed, and only then: another
/// entry of the lock moving leaves it be.
#[test]
fn a_generator_on_a_lock_pin_is_rebuilt_when_the_pin_moves() {
    let server = Server::start();
    server.serve("util.wado", UTIL.as_bytes());
    let project = generator_package(&server.url("util.wado"), "");
    let wado_root = tempfile::tempdir().unwrap();

    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("generated!\n");

    let index = project.path().join("build/kiln/generators.json");
    let built = std::fs::metadata(&index).unwrap().modified().unwrap();
    let lock = project.path().join("wado.lock");
    let other = format!(
        "{}\n[[remote]]\nurl = \"{}\"\nintegrity = \"{}\"\n",
        std::fs::read_to_string(&lock).unwrap(),
        server.url("other.wado"),
        Integrity::of(b"")
    );
    std::fs::write(&lock, other).unwrap();
    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("generated!\n");
    assert_eq!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        built,
        "the generator was rebuilt"
    );

    server.serve(
        "util.wado",
        b"pub fn exclaim(s: String) -> String { return `${s}?`; }\n",
    );
    wado(project.path(), wado_root.path())
        .arg("update")
        .assert()
        .success();
    wado(project.path(), wado_root.path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("generated?\n");
    assert_ne!(
        std::fs::metadata(&index).unwrap().modified().unwrap(),
        built
    );
}

/// A server that sends the whole file for a range is asked once for the probe,
/// however many blocks the header spans, and its answer is read only as far as
/// the probe reads.
#[test]
fn a_server_ignoring_range_is_read_once_and_only_as_far_as_the_probe_reads() {
    let server = Server::start_with(Ranges::Ignored);
    let mut model = vec![b' '; 70_000];
    model.extend_from_slice(b"7\n");
    // Far past what socket buffers hold, so an unread rest cannot be sent.
    model.resize(64 << 20, b'x');
    server.serve("model.bin", &model);
    let project = model_package(&server);

    wado(project.path(), tempfile::tempdir().unwrap().path())
        .args(["run", "src/main.wado"])
        .assert()
        .success()
        .stdout("7\n");
    // One read for the probe and one for the extent it declared.
    let requests = server.settled_requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(
        requests.iter().all(|r| r.sent_whole == Some(false)),
        "{requests:?}"
    );
}

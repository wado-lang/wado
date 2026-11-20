//! Native libraries a runner fetches the first time it needs one, held to the
//! digest of each file it takes out of a pinned archive, and kept in the user
//! cache. A runner loads such a library rather than linking it, so no build
//! depends on one, and a user names none to run on the default.

use std::fmt::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail, ensure};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};

use crate::atomic_write::{hold, write_atomic};
use crate::tls_trust::reqwest_client;
use crate::user_config::xdg_base_dir;

/// An archive, and the files a runner takes out of it.
pub struct Pinned {
    /// What the files are, for the line that says a fetch is under way.
    pub name: &'static str,
    pub url: &'static str,
    pub archive: Archive,
    pub members: &'static [Member],
}

pub enum Archive {
    /// Read whole, since a gzip stream has no index to seek by, and held to
    /// its own pin before any of it is parsed.
    TarGz { sha256: &'static str },
    /// Read a member at a time, by `Range`, from its central directory.
    Zip,
}

/// A file of the archive, kept under its file name beside the others.
pub struct Member {
    pub path: &'static str,
    pub sha256: &'static str,
}

impl Member {
    /// What [`Pinned::dir`] keeps the member under.
    pub fn file_name(&self) -> &'static str {
        self.path.rsplit('/').next().unwrap_or(self.path)
    }
}

impl Pinned {
    /// The directory holding every member under its file name, fetched into the
    /// user cache the first time.
    ///
    /// # Errors
    ///
    /// The fetch fails, or the archive or a member does not hash to its pin.
    pub async fn dir(&self) -> Result<PathBuf> {
        let dir = self.cache_dir()?;
        if self.is_cached(&dir) {
            return Ok(dir);
        }
        // Processes started together fetch once: the rest wait, then find it.
        let lock = dir.with_extension("lock");
        let _held = hold(&lock).with_context(|| format!("cannot hold {}", lock.display()))?;
        if self.is_cached(&dir) {
            return Ok(dir);
        }
        eprintln!(
            "info: fetching {} once, into {}, from {}",
            self.name,
            dir.display(),
            self.url
        );
        let client = reqwest_client()?.build()?;
        let files = match self.archive {
            Archive::TarGz { sha256 } => {
                let gzip = get(&client, self.url, Part::Whole).await?.bytes;
                check(&gzip, sha256, self.url)?;
                tar_members(&gunzip(&gzip)?, self.members)?
            }
            Archive::Zip => zip_members(&client, self.url, self.members).await?,
        };
        for (member, bytes) in self.members.iter().zip(files) {
            check(
                &bytes,
                member.sha256,
                &format!("{} in {}", member.path, self.url),
            )?;
            let path = dir.join(member.file_name());
            write_atomic(&path, &bytes)
                .with_context(|| format!("cannot write {}", path.display()))?;
        }
        Ok(dir)
    }

    fn is_cached(&self, dir: &Path) -> bool {
        self.members
            .iter()
            .all(|member| dir.join(member.file_name()).is_file())
    }

    /// Keyed by the members' pins, so a new pin never finds an old file.
    fn cache_dir(&self) -> Result<PathBuf> {
        let pins: Vec<&str> = self.members.iter().map(|member| member.sha256).collect();
        let key = sha256_hex(pins.join("\n").as_bytes());
        let cache = xdg_base_dir("XDG_CACHE_HOME", ".cache")
            .context("cannot locate the home directory; set XDG_CACHE_HOME")?;
        Ok(cache.join("wado").join("pinned").join(key))
    }
}

fn check(bytes: &[u8], pin: &str, what: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    ensure!(
        actual == pin,
        "{what} hashes to sha256:{actual}, not its pin sha256:{pin}"
    );
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// What part of a file a GET asks for.
enum Part {
    Whole,
    /// The last bytes, as many as the file has up to this.
    Tail(u64),
    Span(Range<u64>),
}

impl Part {
    /// This part of `bytes`, a whole file, clamped to its end.
    fn of(&self, bytes: &[u8]) -> Vec<u8> {
        let len = bytes.len() as u64;
        let Range { start, end } = match self {
            Self::Whole => 0..len,
            Self::Tail(n) => len.saturating_sub(*n)..len,
            Self::Span(span) => span.start.min(len)..span.end.min(len),
        };
        bytes[start as usize..end as usize].to_vec()
    }
}

/// What a GET answered, and the length of the whole file.
struct Fetched {
    bytes: Vec<u8>,
    total: u64,
}

/// GET `part` of `url`, again after a connection that fails. A server that
/// ignores `Range` is answered from the whole file it sends.
async fn get(client: &Client, url: &str, part: Part) -> Result<Fetched> {
    const ATTEMPTS: usize = 3;
    let mut attempt = 1;
    let (status, total, bytes) = loop {
        match get_once(client, url, &part).await {
            Ok(answered) => break answered,
            Err(e) if attempt < ATTEMPTS => {
                eprintln!("info: fetching {url} again after: {e}");
                attempt += 1;
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("cannot fetch {url}"))),
        }
    };
    match (status, total) {
        (StatusCode::PARTIAL_CONTENT, Some(total)) => {
            ensure!(
                bytes.len() as u64 <= total,
                "cannot fetch {url}: the server sent more than the file it names"
            );
            Ok(Fetched { bytes, total })
        }
        (StatusCode::OK, _) => Ok(Fetched {
            total: bytes.len() as u64,
            bytes: part.of(&bytes),
        }),
        (status, _) => bail!("cannot fetch {url}: the server answered {status}"),
    }
}

/// What one GET answered: its status, the file's length where the answer is
/// a range, and the body.
async fn get_once(
    client: &Client,
    url: &str,
    part: &Part,
) -> Result<(StatusCode, Option<u64>, Vec<u8>), reqwest::Error> {
    let mut request = client.get(url);
    match part {
        Part::Whole => {}
        Part::Tail(n) => request = request.header(RANGE, format!("bytes=-{n}")),
        Part::Span(span) => {
            request = request.header(RANGE, format!("bytes={}-{}", span.start, span.end - 1));
        }
    }
    let response = request.send().await?;
    let status = response.status();
    let total = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit_once('/'))
        .and_then(|(_, total)| total.parse().ok());
    Ok((status, total, response.bytes().await?.to_vec()))
}

/// The one member of a gzip stream, inflated whole. Its trailer gives the
/// length modulo 2³², which a member under 4 GiB is.
fn gunzip(gzip: &[u8]) -> Result<Vec<u8>> {
    let trailer: [u8; 4] = gzip
        .last_chunk()
        .copied()
        .context("the archive is too short to be gzip")?;
    let mut out = vec![0; u32::from_le_bytes(trailer) as usize];
    inflate(gzip, &mut out, GZIP_WINDOW_BITS)?;
    Ok(out)
}

/// zlib's `windowBits` for a gzip header and trailer, and for raw deflate.
const GZIP_WINDOW_BITS: i32 = 16 + 15;
const RAW_WINDOW_BITS: i32 = -15;

fn inflate(input: &[u8], out: &mut [u8], window_bits: i32) -> Result<()> {
    let expected = out.len();
    let (inflated, code) =
        zlib_rs::decompress_slice(out, input, zlib_rs::InflateConfig { window_bits });
    ensure!(
        code == zlib_rs::ReturnCode::Ok && inflated.len() == expected,
        "the archive does not inflate ({code:?})"
    );
    Ok(())
}

/// The `members` of a ustar archive, in their order.
fn tar_members(tar: &[u8], members: &[Member]) -> Result<Vec<Vec<u8>>> {
    const BLOCK: usize = 512;
    let mut found: Vec<Option<Vec<u8>>> = vec![None; members.len()];
    let mut at = 0;
    while let Some(header) = tar.get(at..at + BLOCK)
        && header.iter().any(|&b| b != 0)
    {
        let text = |field: Range<usize>| {
            let field = &header[field];
            let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
            String::from_utf8_lossy(&field[..end]).trim().to_owned()
        };
        ensure!(
            header[257..262] == *b"ustar",
            "the archive is not a ustar tar"
        );
        let size = usize::from_str_radix(&text(124..136), 8).context("a tar size is not octal")?;
        // GNU's header keeps times where POSIX's keeps the path's prefix.
        let prefix = if header[257..263] == *b"ustar\0" {
            text(345..500)
        } else {
            String::new()
        };
        let name = text(0..100);
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let data = at + BLOCK;
        let body = tar
            .get(data..data + size)
            .context("the tar archive ends inside a file")?;
        let path = path.trim_start_matches("./");
        let is_file = matches!(header[156], b'0' | 0);
        if is_file && let Some(i) = members.iter().position(|member| member.path == path) {
            found[i] = Some(body.to_vec());
        }
        at = data + size.div_ceil(BLOCK) * BLOCK;
    }
    collect(found, members)
}

fn collect(found: Vec<Option<Vec<u8>>>, members: &[Member]) -> Result<Vec<Vec<u8>>> {
    found
        .into_iter()
        .zip(members)
        .map(|(bytes, member)| {
            bytes.with_context(|| format!("the archive holds no {}", member.path))
        })
        .collect()
}

fn u16_at(bytes: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([
        bytes[at],
        bytes[at + 1],
        bytes[at + 2],
        bytes[at + 3],
    ]))
}

/// Where a member's local header starts, how it is stored, and its sizes.
struct ZipEntry {
    offset: u64,
    method: usize,
    compressed: u64,
    size: u64,
}

/// The `members` of the zip at `url`, each fetched by its own range: an
/// archive can be far larger than what is taken out of it.
async fn zip_members(client: &Client, url: &str, members: &[Member]) -> Result<Vec<Vec<u8>>> {
    let tail = get(client, url, Part::Tail(ZIP_TAIL)).await?;
    let tail_at = tail.total - tail.bytes.len() as u64;
    let directory = match zip_directory_in(&tail.bytes, tail_at)? {
        Ok(directory) => directory.to_vec(),
        Err(span) => get(client, url, Part::Span(span)).await?.bytes,
    };
    let entries = zip_directory(&directory)?;
    let mut found = Vec::new();
    for member in members {
        let Some(entry) = entries
            .iter()
            .find_map(|(path, entry)| (path == member.path).then_some(entry))
        else {
            found.push(None);
            continue;
        };
        // The local header repeats the name and carries an extra field of its
        // own, each at most 64 KiB, so one range covering both reaches the data.
        let span = entry.offset..entry.offset + ZIP_LOCAL_FIXED + 2 * 0xffff + entry.compressed;
        let local = get(client, url, Part::Span(span)).await?.bytes;
        found.push(Some(zip_data(&local, entry)?));
    }
    collect(found, members)
}

const ZIP_END_LEN: usize = 22;
/// What holds the end record, which a comment of at most 64 KiB may follow.
const ZIP_TAIL: u64 = (ZIP_END_LEN + 0xffff) as u64;
const ZIP_LOCAL_FIXED: u64 = 30;

/// The central directory, out of `tail`, the end of the archive from `tail_at`
/// on, or else the span of the archive it is at.
fn zip_directory_in(tail: &[u8], tail_at: u64) -> Result<Result<&[u8], Range<u64>>> {
    const END_SIGNATURE: &[u8] = b"PK\x05\x06";
    // The record is the one whose comment ends the archive: a comment may hold
    // the signature's bytes too.
    let record = (0..tail.len().saturating_sub(ZIP_END_LEN - 1))
        .rev()
        .map(|end| &tail[end..])
        .find(|rest| {
            rest.starts_with(END_SIGNATURE) && ZIP_END_LEN + u16_at(rest, 20) == rest.len()
        })
        .context("the archive has no zip end record")?;
    let len = u32_at(record, 12);
    let at = u32_at(record, 16);
    ensure!(len > 0, "the zip archive holds no files");
    ensure!(
        at != 0xffff_ffff,
        "the archive is zip64, which this reader does not take"
    );
    if at < tail_at {
        return Ok(Err(at..at + len));
    }
    let start = usize::try_from(at - tail_at)?;
    Ok(Ok(tail.get(start..start + usize::try_from(len)?).context(
        "the zip central directory runs past the end record",
    )?))
}

/// Every entry of a zip's central directory, by path.
fn zip_directory(directory: &[u8]) -> Result<Vec<(String, ZipEntry)>> {
    const SIGNATURE: &[u8] = b"PK\x01\x02";
    const FIXED: usize = 46;
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some(fixed) = directory.get(at..at + FIXED) {
        ensure!(
            &fixed[..4] == SIGNATURE,
            "the zip central directory is malformed"
        );
        let name_len = u16_at(fixed, 28);
        let name = directory
            .get(at + FIXED..at + FIXED + name_len)
            .context("the zip central directory is cut short")?;
        entries.push((
            String::from_utf8_lossy(name).into_owned(),
            ZipEntry {
                offset: u32_at(fixed, 42),
                method: u16_at(fixed, 10),
                compressed: u32_at(fixed, 20),
                size: u32_at(fixed, 24),
            },
        ));
        at += FIXED + name_len + u16_at(fixed, 30) + u16_at(fixed, 32);
    }
    Ok(entries)
}

/// The member `entry` names, out of `local`, the archive from its local header.
fn zip_data(local: &[u8], entry: &ZipEntry) -> Result<Vec<u8>> {
    const FIXED: usize = ZIP_LOCAL_FIXED as usize;
    const STORED: usize = 0;
    const DEFLATED: usize = 8;
    let header = local
        .get(..FIXED)
        .context("the zip local header is cut short")?;
    let data_at = FIXED + u16_at(header, 26) + u16_at(header, 28);
    let data = local
        .get(data_at..data_at + usize::try_from(entry.compressed)?)
        .context("the zip member is cut short")?;
    match entry.method {
        STORED => Ok(data.to_vec()),
        DEFLATED => {
            // Deflate expands a byte to at most 1032.
            ensure!(
                entry.size <= entry.compressed.saturating_mul(1032),
                "a zip member claims more than deflate can expand to"
            );
            let mut out = vec![0; usize::try_from(entry.size)?];
            inflate(data, &mut out, RAW_WINDOW_BITS)?;
            Ok(out)
        }
        method => bail!("a zip member stored by method {method} is not one this reader takes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMBER: Member = Member {
        path: "lib/a.so",
        sha256: "",
    };

    /// A header's type, its magic, and what it keeps at the prefix's offset.
    struct Entry<'a> {
        name: &'a str,
        body: &'a [u8],
        typeflag: u8,
        magic: &'a [u8; 8],
        prefix: &'a str,
    }

    const POSIX: &[u8; 8] = b"ustar\x0000";
    const GNU: &[u8; 8] = b"ustar  \0";

    fn file<'a>(name: &'a str, body: &'a [u8]) -> Entry<'a> {
        Entry {
            name,
            body,
            typeflag: b'0',
            magic: POSIX,
            prefix: "",
        }
    }

    fn ustar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let entries: Vec<_> = entries
            .iter()
            .map(|&(name, body)| file(name, body))
            .collect();
        tar_of(&entries)
    }

    fn tar_of(entries: &[Entry]) -> Vec<u8> {
        let mut tar = Vec::new();
        for entry in entries {
            let mut header = [0u8; 512];
            header[..entry.name.len()].copy_from_slice(entry.name.as_bytes());
            let size = format!("{:011o}\0", entry.body.len());
            header[124..136].copy_from_slice(size.as_bytes());
            header[156] = entry.typeflag;
            header[257..265].copy_from_slice(entry.magic);
            header[345..345 + entry.prefix.len()].copy_from_slice(entry.prefix.as_bytes());
            tar.extend_from_slice(&header);
            let body = entry.body;
            tar.extend_from_slice(body);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.extend_from_slice(&[0; 1024]);
        tar
    }

    #[test]
    fn a_tar_member_is_found_past_others_and_a_leading_dot() {
        let tar = ustar(&[("./other", &[1; 600]), ("./lib/a.so", b"lib")]);
        assert_eq!(tar_members(&tar, &[MEMBER]).unwrap(), [b"lib".to_vec()]);
    }

    #[test]
    fn a_tar_member_is_a_regular_file_at_the_path_its_prefix_completes() {
        let link = Entry {
            typeflag: b'2',
            ..file("lib/a.so", b"")
        };
        let prefixed = Entry {
            prefix: "lib",
            ..file("a.so", b"lib")
        };
        let tar = tar_of(&[link, prefixed]);
        assert_eq!(tar_members(&tar, &[MEMBER]).unwrap(), [b"lib".to_vec()]);
    }

    #[test]
    fn a_gnu_header_keeps_no_prefix() {
        let gnu = Entry {
            magic: GNU,
            prefix: "15243064557",
            ..file("lib/a.so", b"lib")
        };
        assert_eq!(
            tar_members(&tar_of(&[gnu]), &[MEMBER]).unwrap(),
            [b"lib".to_vec()]
        );
    }

    #[test]
    fn a_tar_without_the_member_names_it() {
        let tar = ustar(&[("./other", b"x")]);
        let error = tar_members(&tar, &[MEMBER]).unwrap_err().to_string();
        assert!(error.contains("lib/a.so"), "{error}");
    }

    fn compressed(data: &[u8], window_bits: i32) -> Vec<u8> {
        let mut out = vec![0; zlib_rs::compress_bound(data.len()) + 32];
        let config = zlib_rs::DeflateConfig {
            window_bits,
            ..zlib_rs::DeflateConfig::default()
        };
        let (written, code) = zlib_rs::compress_slice(&mut out, data, config);
        assert_eq!(code, zlib_rs::ReturnCode::Ok);
        written.to_vec()
    }

    /// A zip of `entries`, each deflated, and a local extra field the central
    /// directory does not repeat, as some writers leave.
    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let le16 = |n: usize| (n as u16).to_le_bytes();
        let le32 = |n: usize| (n as u32).to_le_bytes();
        let mut archive = Vec::new();
        let mut directory = Vec::new();
        for (name, body) in entries {
            let data = compressed(body, RAW_WINDOW_BITS);
            let offset = archive.len();
            let extra = [0u8; 7];
            archive.extend_from_slice(b"PK\x03\x04");
            archive.extend_from_slice(&[0; 22]);
            archive.extend_from_slice(&le16(name.len()));
            archive.extend_from_slice(&le16(extra.len()));
            archive.extend_from_slice(name.as_bytes());
            archive.extend_from_slice(&extra);
            archive.extend_from_slice(&data);
            directory.extend_from_slice(b"PK\x01\x02");
            directory.extend_from_slice(&[0; 6]);
            directory.extend_from_slice(&le16(8));
            directory.extend_from_slice(&[0; 8]);
            directory.extend_from_slice(&le32(data.len()));
            directory.extend_from_slice(&le32(body.len()));
            directory.extend_from_slice(&le16(name.len()));
            directory.extend_from_slice(&[0; 12]);
            directory.extend_from_slice(&le32(offset));
            directory.extend_from_slice(name.as_bytes());
        }
        let directory_at = archive.len();
        archive.extend_from_slice(&directory);
        archive.extend_from_slice(b"PK\x05\x06");
        archive.extend_from_slice(&[0; 8]);
        archive.extend_from_slice(&le32(directory.len()));
        archive.extend_from_slice(&le32(directory_at));
        archive.extend_from_slice(&le16(0));
        archive
    }

    #[test]
    fn a_zip_member_is_read_by_the_spans_its_directory_names() {
        let body = b"a library, a library, a library".repeat(9);
        let archive = zip(&[("other", b"x"), ("lib/a.so", &body)]);
        let tail = Part::Tail(ZIP_TAIL).of(&archive);
        let directory = zip_directory_in(&tail, 0).unwrap().unwrap();
        let entries = zip_directory(directory).unwrap();
        let (_, entry) = entries.iter().find(|(path, _)| path == "lib/a.so").unwrap();
        let start = entry.offset;
        let local = Part::Span(start..start + 4096).of(&archive);
        assert_eq!(zip_data(&local, entry).unwrap(), body);
    }

    #[test]
    fn a_zip_directory_before_the_tail_is_named_by_its_span() {
        let archive = zip(&[("lib/a.so", b"lib")]);
        let tail_at = archive.len() as u64 - 30;
        let tail = Part::Tail(30).of(&archive);
        let span = zip_directory_in(&tail, tail_at).unwrap().unwrap_err();
        assert_eq!(
            &archive[span.start as usize..span.start as usize + 4],
            b"PK\x01\x02"
        );
    }

    #[test]
    fn a_zip_with_an_empty_directory_holds_no_files() {
        let error = zip_directory_in(&zip(&[]), 0).unwrap_err().to_string();
        assert!(error.contains("holds no files"), "{error}");
    }

    #[test]
    fn a_zip_comment_holding_the_end_signature_is_not_the_end_record() {
        let mut archive = zip(&[("lib/a.so", b"lib")]);
        let comment = b"PK\x05\x06 a comment";
        let len_at = archive.len() - 2;
        archive[len_at..].copy_from_slice(&(comment.len() as u16).to_le_bytes());
        archive.extend_from_slice(comment);
        let directory = zip_directory_in(&archive, 0).unwrap().unwrap();
        assert_eq!(zip_directory(directory).unwrap()[0].0, "lib/a.so");
    }

    #[test]
    fn a_zip_member_claiming_more_than_deflate_expands_to_is_refused() {
        let archive = zip(&[("lib/a.so", b"lib")]);
        let directory = zip_directory_in(&archive, 0).unwrap().unwrap();
        let (_, mut entry) = zip_directory(directory).unwrap().remove(0);
        entry.size = entry.compressed * 1032 + 1;
        let error = zip_data(&archive, &entry).unwrap_err().to_string();
        assert!(error.contains("deflate"), "{error}");
    }

    #[test]
    fn a_gzip_stream_inflates_to_the_length_its_trailer_gives() {
        let body = b"a library".repeat(100);
        assert_eq!(gunzip(&compressed(&body, GZIP_WINDOW_BITS)).unwrap(), body);
    }

    #[test]
    fn a_digest_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}

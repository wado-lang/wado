//! Remote files: a module or a Kiln input named by an `https://` URL (or an
//! `http://` one on this machine), fetched, held to a pin, and kept by content.
//!
//! The pin is the `use`'s `integrity`, else the `[[remote]]` entry of the
//! governing package's `wado.lock`. A package with no entry for the URL yet
//! records the digest its first read sees, and holds each later read to it. A
//! file in no package has no lock to record in, so its `use` must pin it.
//!
//! Fetched bytes live at `{root}/remote/sha256/<hex>`, so a pinned read needs
//! the network only the first time any project reads them. A Kiln probe reads
//! the longest pinned part kept there, and the server only past it.

use std::error::Error;
use std::net::{IpAddr, ToSocketAddrs};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::redirect::Policy;
use reqwest::{Certificate, Client, Response, StatusCode, Url};
use wado_compiler::hashmap::IndexMap;
use wado_compiler::remote::{Integrity, PinnedBy, check_digest, url_fault};
use wado_compiler::{SourceError, SourceReader};
use wado_host::atomic_write::write_atomic;
use wado_host::sync::{joined, lock};
use wado_host::tls_trust::extra_ca_certs;
use wado_manifest::{LOCK_FILENAME, LockFile, LockFileError, LockedRemote};

use crate::cache;
use crate::manifest::{discover, hold_lock_file};

/// How much a [`RemoteReader`] fetches at once: a probe reads a header a few
/// bytes at a time, and each read of its own would be a round trip.
const BLOCK: u64 = 1 << 16;

/// Every fetch runs here. A [`SourceReader`] is asked synchronously, from
/// inside whatever runtime drives the compile, where blocking on that runtime
/// is not allowed; a runtime of its own can be waited on from anywhere.
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("wado-remote")
        .enable_all()
        .build()
        .expect("the remote fetch runtime starts")
});

/// Trusts the CAs `wado run` does beyond the platform's, so a host behind a
/// custom or intercepting CA is reached the way a guest's HTTPS is.
static CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    let certs = extra_ca_certs()
        .map(|der| Certificate::from_der(der))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Client::builder()
        .tls_certs_merge(certs)
        .redirect(Policy::custom(|attempt| {
            let from = attempt
                .previous()
                .last()
                .expect("a redirect follows a request");
            let fault = url_fault(attempt.url().as_str())
                .or_else(|| redirect_fault(from, attempt.url(), reach_of));
            if let Some(fault) = fault {
                let target = attempt.url().to_string();
                attempt.error(format!("a redirect to '{target}': {fault}"))
            } else if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error("too many redirects")
            } else {
                attempt.follow()
            }
        }))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
});

const MAX_REDIRECTS: usize = 10;
/// A host that stops answering fails the build rather than hanging it.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Which side of the boundary between this network and the internet a host
/// is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    Internet,
    /// This machine or a network not routed on the internet.
    Intranet,
}

/// Why following a redirect from `from` to `to` is refused, or `None`. A
/// server cannot send the build somewhere its own URL could not have named:
/// off `https://`, or across the boundary `reach` draws, either way.
fn redirect_fault(from: &Url, to: &Url, reach: impl Fn(&Url) -> Reach) -> Option<&'static str> {
    if from.scheme() == "https" && to.scheme() != "https" {
        return Some("it leaves `https://`");
    }
    match (reach(from), reach(to)) {
        (Reach::Internet, Reach::Intranet) => Some("it leads from the internet into this network"),
        (Reach::Intranet, Reach::Internet) => Some("it leads from this network to the internet"),
        _ => None,
    }
}

/// Where `url`'s host is. A name this machine cannot resolve is left to a
/// proxy, which reaches the internet.
///
/// A name is resolved here, not where the connection is made, so a name whose
/// answer changes in between (DNS rebinding) passes. Checking the address
/// connected to would mean replacing the client's resolver, which a proxy
/// bypasses: it resolves the name itself.
fn reach_of(url: &Url) -> Reach {
    let host = url.host_str().unwrap_or_default();
    let addresses: Vec<IpAddr> = match host.trim_matches(['[', ']']).parse() {
        Ok(ip) => vec![ip],
        // The policy is asked synchronously, from a worker of `RUNTIME`.
        Err(_) => tokio::task::block_in_place(|| (host, 0).to_socket_addrs())
            .map(|found| found.map(|address| address.ip()).collect())
            .unwrap_or_default(),
    };
    if addresses.into_iter().any(is_intranet) {
        Reach::Intranet
    } else {
        Reach::Internet
    }
}

fn is_intranet(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => {
            let [a, b, ..] = ip.octets();
            let shared = a == 100 && b & 0xc0 == 64;
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || shared
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_unspecified()
        }
    }
}

/// What one GET answered: the bytes read of it, and the file's whole length
/// where the server said. `whole` where the server ignored `Range` and sent the
/// file from its start, read only as far as the range: `rest` then holds the
/// body past `bytes`, so a caller reads on rather than asking again.
struct Fetched {
    bytes: Vec<u8>,
    total: Option<u64>,
    whole: bool,
    rest: Option<Response>,
}

impl Fetched {
    /// The `range` of the file these bytes hold.
    fn into_range(self, range: Range<u64>) -> Vec<u8> {
        if self.whole {
            part(&self.bytes, range).to_vec()
        } else {
            self.bytes
        }
    }
}

/// GET `url`, or the `range` of it.
async fn get(url: String, range: Option<Range<u64>>) -> Result<Fetched, SourceError> {
    let network = |message: String| SourceError::NetworkError {
        url: url.clone(),
        message,
    };
    let task = {
        let url = url.clone();
        let range = range.clone();
        RUNTIME.spawn(async move { fetch(&url, range).await })
    };
    let has_range = range.is_some();
    let (status, mut fetched) = joined(task.await).map_err(network)?;
    match status {
        StatusCode::NOT_FOUND | StatusCode::GONE => Err(SourceError::NotFound { path: url }),
        // A server that does not say how long the file is was read to its end.
        StatusCode::OK => {
            fetched.total.get_or_insert(fetched.bytes.len() as u64);
            Ok(fetched)
        }
        StatusCode::PARTIAL_CONTENT => Ok(fetched),
        // Past the end: a range the file does not reach holds nothing.
        StatusCode::RANGE_NOT_SATISFIABLE if has_range => Ok(Fetched {
            bytes: Vec::new(),
            ..fetched
        }),
        status => Err(network(format!("the server answered {status}"))),
    }
}

async fn fetch(url: &str, range: Option<Range<u64>>) -> Result<(StatusCode, Fetched), String> {
    if let Some(fault) = url_fault(url) {
        return Err(fault.to_string());
    }
    let client = CLIENT.as_ref().map_err(Clone::clone)?;
    let mut request = client.get(url);
    if let Some(range) = &range {
        request = request.header(RANGE, format!("bytes={}-{}", range.start, range.end - 1));
    }
    let response = request.send().await.map_err(|e| reason(&e))?;
    let status = response.status();
    let total = match status {
        StatusCode::PARTIAL_CONTENT | StatusCode::RANGE_NOT_SATISFIABLE => response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit_once('/'))
            .and_then(|(_, total)| total.parse().ok()),
        _ => response.content_length(),
    };
    // The whole file sent for a range is read only as far as the range.
    let upto = match (&range, total) {
        (Some(range), Some(_)) => range.end,
        _ => u64::MAX,
    };
    let mut bytes = Vec::new();
    let rest = read_body(response, &mut bytes, upto).await?;
    Ok((
        status,
        Fetched {
            bytes,
            total,
            // A server that ignores `Range` sends the whole file.
            whole: status == StatusCode::OK,
            rest,
        },
    ))
}

/// The `range` of `bytes`, clamped to their end.
fn part(bytes: &[u8], range: Range<u64>) -> &[u8] {
    let clamp = |at: u64| usize::try_from(at).map_or(bytes.len(), |at| at.min(bytes.len()));
    &bytes[clamp(range.start)..clamp(range.end)]
}

/// Read `response`'s body onto `bytes` until they hold `upto` bytes, handing
/// the response back where its body goes on.
async fn read_body(
    mut response: Response,
    bytes: &mut Vec<u8>,
    upto: u64,
) -> Result<Option<Response>, String> {
    while (bytes.len() as u64) < upto {
        let Some(chunk) = response.chunk().await.map_err(|e| reason(&e))? else {
            return Ok(None);
        };
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(response))
}

/// `error` with every error under it: reqwest's own message names none.
fn reason(error: &reqwest::Error) -> String {
    std::iter::successors(Some(error as &dyn Error), |&e| e.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

/// The first `extent` bytes of `url`, or all of it, held to `use_pin` or to
/// what the `wado.lock` of the package at `lock_root` pins.
///
/// # Errors
///
/// The fetch's failure, [`SourceError::IntegrityMismatch`] where the bytes
/// hash to something other than their pin, and [`SourceError::Unpinned`] where
/// nothing pins them and there is no lock to record in.
pub async fn load(
    url: &str,
    extent: Option<u64>,
    use_pin: Option<Integrity>,
    lock_root: Option<&Path>,
) -> Result<Vec<u8>, SourceError> {
    let pins = Pins::of(url, use_pin, lock_root)?;
    let pin = pins.at(extent);
    if let Some((pin, _)) = pin
        && let Some(bytes) = cached(pin)
    {
        return Ok(bytes);
    }
    let (bytes, len) = fetch_prefix(url, extent).await?;
    let actual = Integrity::of(&bytes);
    match (pin, lock_root) {
        (Some((pin, pinned_by)), _) => check_digest(url, actual, pin, pinned_by)?,
        (None, Some(root)) => {
            hold_to_other_extents(url, extent, &bytes, &pins).await?;
            record_pin(root, url, extent, actual)?;
        }
        (None, None) => {
            return Err(SourceError::Unpinned {
                url: url.to_string(),
                actual,
            });
        }
    }
    store(url, actual, &bytes, len);
    Ok(bytes)
}

/// `url` opened to be read a range at a time: from the bytes kept for the
/// longest part of it `use_pin` or the lock at `lock_root` pins, and past them
/// from the server, which a probe reading only what a pin covers never asks.
///
/// # Errors
///
/// A lock that cannot be read, and the fetch's failure where nothing is kept.
pub async fn open(
    url: &str,
    use_pin: Option<Integrity>,
    lock_root: Option<&Path>,
) -> Result<Arc<dyn SourceReader>, SourceError> {
    let kept = Pins::of(url, use_pin, lock_root)?
        .all()
        .into_iter()
        .filter_map(|pin| Some((cached(pin)?, kept_len(url, pin)?)))
        .max_by_key(|(bytes, _)| bytes.len());
    Ok(match kept {
        Some((bytes, len)) if bytes.len() as u64 == len => Arc::new(bytes),
        Some((kept, len)) => Arc::new(KeptPrefix {
            url: url.to_string(),
            kept,
            len,
            rest: Mutex::new(None),
        }),
        None => Arc::new(RemoteReader::open(url).await?),
    })
}

/// What pins `url`: the `use`'s pin, which holds whatever extent is read, else
/// each extent the lock pins.
enum Pins {
    Use(Integrity),
    Lock(Vec<(Option<u64>, Integrity)>),
}

impl Pins {
    /// What pins `url` for a `use` writing `use_pin`, in the package at
    /// `lock_root`.
    fn of(
        url: &str,
        use_pin: Option<Integrity>,
        lock_root: Option<&Path>,
    ) -> Result<Self, SourceError> {
        if let Some(pin) = use_pin {
            return Ok(Self::Use(pin));
        }
        let lock = match lock_root {
            Some(root) => read_lock(root)?.map(|lock| pins_of(&lock, root, url)),
            None => None,
        };
        Ok(Self::Lock(lock.transpose()?.unwrap_or_default()))
    }

    /// The pin of the first `extent` bytes, and who wrote it.
    fn at(&self, extent: Option<u64>) -> Option<(Integrity, PinnedBy)> {
        match self {
            Self::Use(pin) => Some((*pin, PinnedBy::Use)),
            Self::Lock(pins) => pins
                .iter()
                .find_map(|&(other, pin)| (other == extent).then_some((pin, PinnedBy::Lock))),
        }
    }

    fn all(&self) -> Vec<Integrity> {
        match self {
            Self::Use(pin) => vec![*pin],
            Self::Lock(pins) => pins.iter().map(|&(_, pin)| pin).collect(),
        }
    }
}

/// The first `extent` bytes of `url`, or all of it, and the file's length where
/// the server said.
async fn fetch_prefix(
    url: &str,
    extent: Option<u64>,
) -> Result<(Vec<u8>, Option<u64>), SourceError> {
    Ok(match extent {
        Some(0) => (Vec::new(), None),
        Some(extent) => {
            let fetched = get(url.to_string(), Some(0..extent)).await?;
            let len = fetched.total;
            (fetched.into_range(0..extent), len)
        }
        None => {
            let fetched = get(url.to_string(), None).await?;
            (fetched.bytes, fetched.total)
        }
    })
}

/// Hold `bytes`, the first `extent` bytes of `url`, to every pin `pins` holds
/// for another extent of it. A probe's extent is decided by the bytes inside
/// it, so content that moved the extent changed, and an entry of its own would
/// accept it unasked.
async fn hold_to_other_extents(
    url: &str,
    extent: Option<u64>,
    bytes: &[u8],
    pins: &Pins,
) -> Result<(), SourceError> {
    let Pins::Lock(pins) = pins else {
        unreachable!("a `use` pins every extent it reads");
    };
    for &(other, pin) in pins {
        let digest = match other {
            Some(other) if extent.is_none_or(|extent| other <= extent) => {
                Integrity::of(part(bytes, 0..other))
            }
            _ if cached(pin).is_some_and(|kept| kept.starts_with(bytes)) => continue,
            _ => Integrity::of(&fetch_prefix(url, other).await?.0),
        };
        check_digest(url, digest, pin, PinnedBy::Lock)?;
    }
    Ok(())
}

fn lock_error(root: &Path, message: String) -> SourceError {
    SourceError::IoError {
        path: root.join(LOCK_FILENAME).display().to_string(),
        message,
    }
}

/// The lock of the package at `root`, `None` only where there is none: a lock
/// that cannot be read is an error, never one to write afresh.
fn read_lock(root: &Path) -> Result<Option<LockFile>, SourceError> {
    let text = match std::fs::read_to_string(root.join(LOCK_FILENAME)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(lock_error(root, e.to_string())),
    };
    text.parse()
        .map(Some)
        .map_err(|e: LockFileError| lock_error(root, e.to_string()))
}

/// Every extent of `url` that `lock`, the lock of the package at `root`, pins,
/// with its pin.
fn pins_of(
    lock: &LockFile,
    root: &Path,
    url: &str,
) -> Result<Vec<(Option<u64>, Integrity)>, SourceError> {
    lock.remotes
        .iter()
        .filter(|entry| entry.url == url)
        .map(|entry| {
            let pin = Integrity::parse(&entry.integrity).ok_or_else(|| {
                lock_error(
                    root,
                    format!("the pin of '{url}' is not {}", Integrity::FORM),
                )
            })?;
            Ok((entry.extent, pin))
        })
        .collect()
}

/// What the `wado.lock` of the package at `root` pins `url`'s first `extent`
/// bytes to.
///
/// # Errors
///
/// A lock that cannot be read, or that writes a malformed pin for `url`.
pub fn locked_pin(
    root: &Path,
    url: &str,
    extent: Option<u64>,
) -> Result<Option<Integrity>, SourceError> {
    Ok(Pins::of(url, None, Some(root))?
        .at(extent)
        .map(|(pin, _)| pin))
}

/// Record `actual` as the pin of `url`'s first `extent` bytes, unless another
/// read recorded one first, which `actual` is then held to.
fn record_pin(
    root: &Path,
    url: &str,
    extent: Option<u64>,
    actual: Integrity,
) -> Result<(), SourceError> {
    let _held = hold_lock_file(root).map_err(|message| lock_error(root, message))?;
    let mut lock_file = match read_lock(root)? {
        Some(lock_file) => {
            if let Some((pin, pinned_by)) = Pins::Lock(pins_of(&lock_file, root, url)?).at(extent) {
                return check_digest(url, actual, pin, pinned_by);
            }
            lock_file
        }
        None => LockFile {
            version: 1,
            deps_hash: discover(root)
                .map_err(|e| lock_error(root, e.to_string()))?
                .expect("a lock root holds a `wado.toml`")
                .manifest
                .deps_hash(),
            packages: Vec::new(),
            build_dependencies: Vec::new(),
            remotes: Vec::new(),
        },
    };
    lock_file.remotes.push(LockedRemote {
        url: url.to_string(),
        extent,
        integrity: actual.to_string(),
    });
    write_atomic(&root.join(LOCK_FILENAME), lock_file.to_toml().as_bytes())
        .map_err(|e| lock_error(root, e.to_string()))
}

fn blob_path(integrity: Integrity) -> Option<PathBuf> {
    let root = cache::root().ok()?;
    Some(root.join("remote").join("sha256").join(integrity.hex()))
}

/// Where the length of the file at `url` is kept for the bytes `integrity`
/// pins of it. Keyed by the URL too, as one prefix may begin files of any
/// length.
fn len_path(url: &str, integrity: Integrity) -> Option<PathBuf> {
    let root = cache::root().ok()?;
    let name = format!(
        "{}-{}",
        Integrity::of(url.as_bytes()).hex(),
        integrity.hex()
    );
    Some(root.join("remote").join("len").join(name))
}

/// The bytes kept for `pin`, when they still hash to it.
fn cached(pin: Integrity) -> Option<Vec<u8>> {
    let bytes = std::fs::read(blob_path(pin)?).ok()?;
    (Integrity::of(&bytes) == pin).then_some(bytes)
}

/// The length of the file at `url` when it served the bytes `pin` pins. Only
/// a probe reads it, and the bytes it then depends on are held to their pin,
/// so a wrong length costs a fetch or fails the build, never its output.
fn kept_len(url: &str, pin: Integrity) -> Option<u64> {
    std::fs::read_to_string(len_path(url, pin)?)
        .ok()?
        .parse()
        .ok()
}

/// Keep `bytes`, fetched from `url`, for the next read pinned to them, with
/// `len`, the file's length where the server said. A cache that cannot be
/// written costs only a fetch.
fn store(url: &str, integrity: Integrity, bytes: &[u8], len: Option<u64>) {
    if let Some(path) = blob_path(integrity) {
        let _ = write_atomic(&path, bytes);
    }
    if let (Some(path), Some(len)) = (len_path(url, integrity), len) {
        let _ = write_atomic(&path, len.to_string().as_bytes());
    }
}

/// A remote file whose leading bytes `kept` are served from the cache. A read
/// past them opens the file on the server.
struct KeptPrefix {
    url: String,
    kept: Vec<u8>,
    len: u64,
    rest: Mutex<Option<Arc<RemoteReader>>>,
}

impl KeptPrefix {
    fn rest(&self) -> Result<Arc<RemoteReader>, SourceError> {
        let mut rest = lock(&self.rest);
        if let Some(reader) = &*rest {
            return Ok(Arc::clone(reader));
        }
        let reader = Arc::new(futures::executor::block_on(RemoteReader::open(&self.url))?);
        *rest = Some(Arc::clone(&reader));
        Ok(reader)
    }
}

impl SourceReader for KeptPrefix {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, start: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        let kept = part(&self.kept, start..start + buf.len() as u64);
        buf[..kept.len()].copy_from_slice(kept);
        if kept.len() == buf.len() {
            return Ok(kept.len());
        }
        let past = start.max(self.kept.len() as u64);
        Ok(kept.len() + self.rest()?.read_at(past, &mut buf[kept.len()..])?)
    }
}

/// A remote file read a range at a time, as a Kiln probe reads one.
pub struct RemoteReader {
    url: String,
    len: u64,
    blocks: Mutex<Blocks>,
}

enum Blocks {
    /// Each [`BLOCK`] a read touches, fetched once.
    Ranged(IndexMap<u64, Arc<Vec<u8>>>),
    /// What a server ignoring `Range` sent of the file from its start, its
    /// body read on only as far as a read reaches.
    Whole {
        bytes: Vec<u8>,
        rest: Option<Response>,
    },
}

impl RemoteReader {
    /// Open `url`, learning its length from the first block.
    ///
    /// # Errors
    ///
    /// The fetch's failure, or a server that does not say how long the file is.
    pub async fn open(url: &str) -> Result<Self, SourceError> {
        let first = get(url.to_string(), Some(0..BLOCK)).await?;
        let len = first.total.ok_or_else(|| SourceError::NetworkError {
            url: url.to_string(),
            message: "the server does not say how long the file is".to_string(),
        })?;
        let blocks = if first.whole {
            Blocks::Whole {
                bytes: first.bytes,
                rest: first.rest,
            }
        } else {
            Blocks::Ranged(IndexMap::from_iter([(0, Arc::new(first.bytes))]))
        };
        Ok(Self {
            url: url.to_string(),
            len,
            blocks: Mutex::new(blocks),
        })
    }

    fn block(&self, index: u64) -> Result<Arc<Vec<u8>>, SourceError> {
        if let Some(block) = self.ranged(|blocks| blocks.get(&index).cloned()) {
            return Ok(block);
        }
        let range = index * BLOCK..(index + 1) * BLOCK;
        let fetched = futures::executor::block_on(get(self.url.clone(), Some(range.clone())))?;
        let block = Arc::new(fetched.into_range(range));
        self.ranged(|blocks| blocks.insert(index, Arc::clone(&block)));
        Ok(block)
    }

    fn ranged<T>(&self, f: impl FnOnce(&mut IndexMap<u64, Arc<Vec<u8>>>) -> T) -> T {
        let Blocks::Ranged(blocks) = &mut *lock(&self.blocks) else {
            unreachable!("only a server honoring `Range` is read by block");
        };
        f(blocks)
    }

    /// Read `rest` on until `bytes` hold `upto` bytes or the body ends.
    fn read_on(
        &self,
        rest: Response,
        bytes: &mut Vec<u8>,
        upto: u64,
    ) -> Result<Option<Response>, SourceError> {
        let mut held = std::mem::take(bytes);
        let task = RUNTIME.spawn(async move {
            let rest = read_body(rest, &mut held, upto).await;
            (held, rest)
        });
        let (held, rest) = joined(futures::executor::block_on(task));
        *bytes = held;
        rest.map_err(|message| SourceError::NetworkError {
            url: self.url.clone(),
            message,
        })
    }
}

impl SourceReader for RemoteReader {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, start: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
        if let Blocks::Whole { bytes, rest } = &mut *lock(&self.blocks) {
            let end = start + buf.len() as u64;
            if (bytes.len() as u64) < end
                && let Some(response) = rest.take()
            {
                *rest = self.read_on(response, bytes, end)?;
            }
            let part = part(bytes, start..end);
            buf[..part.len()].copy_from_slice(part);
            return Ok(part.len());
        }
        let mut read = 0;
        while read < buf.len() {
            let at = start + read as u64;
            let block = self.block(at / BLOCK)?;
            let offset = usize::try_from(at % BLOCK).expect("a block offset fits");
            if offset >= block.len() {
                break;
            }
            let n = (buf.len() - read).min(block.len() - offset);
            buf[read..read + n].copy_from_slice(&block[offset..offset + n]);
            read += n;
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        text.parse().unwrap()
    }

    #[test]
    fn a_redirect_stays_on_https_and_on_its_side_of_the_boundary() {
        let reach = |url: &Url| match url.host_str() {
            Some("intranet.test" | "127.0.0.1") => Reach::Intranet,
            _ => Reach::Internet,
        };
        let fault = |from: &str, to: &str| redirect_fault(&url(from), &url(to), reach);
        assert_eq!(fault("https://a.test/m", "https://cdn.test/m"), None);
        assert_eq!(fault("http://127.0.0.1/m", "http://intranet.test/m"), None);
        assert!(fault("https://a.test/m", "http://a.test/m").is_some());
        assert!(fault("https://a.test/m", "https://intranet.test/m").is_some());
        assert!(fault("https://intranet.test/m", "https://a.test/m").is_some());
    }

    #[test]
    fn an_address_the_internet_does_not_route_is_intranet() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_intranet(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.128.0.1", "2001:4860:4860::8888"] {
            assert!(!is_intranet(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(reach_of(&url("http://[::1]:8080/m")), Reach::Intranet);
    }
}

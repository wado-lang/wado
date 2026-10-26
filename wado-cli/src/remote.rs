//! Remote files: a module or a Kiln input named by an `https://` URL (or an
//! `http://` one on this machine), fetched, held to a pin, and kept by content.
//!
//! The pin is the `use`'s `integrity`, else the `[[remote]]` entry of the
//! governing package's `wado.lock`. A package with no entry for the URL yet
//! records the digest its first read sees, and holds each later read to it. A
//! file in no package has no lock to record in, so its `use` must pin it.
//!
//! Fetched bytes live at `{root}/remote/sha256/<hex>`, so a pinned read needs
//! the network only the first time any project reads them.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Certificate, Client, StatusCode};
use wado_compiler::hashmap::IndexMap;
use wado_compiler::remote::{Integrity, PinnedBy, check_digest};
use wado_compiler::{SourceError, SourceReader};
use wado_host::atomic_write::write_atomic;
use wado_host::sync::{joined, lock};
use wado_host::tls_trust::extra_ca_certs;
use wado_manifest::{LOCK_FILENAME, LockFile, LockFileError, LockedRemote};

use crate::cache;
use crate::manifest::discover;

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
        .build()
        .map_err(|e| e.to_string())
});

/// Serializes each read-modify-write of a `wado.lock`, as the hosts of one run
/// record pins at once.
static LOCK_WRITE: Mutex<()> = Mutex::new(());

/// What one GET answered: the bytes, and the file's whole length where the
/// server said.
struct Fetched {
    bytes: Vec<u8>,
    total: Option<u64>,
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
    let (status, bytes, total) = joined(task.await).map_err(network)?;
    match status {
        StatusCode::NOT_FOUND | StatusCode::GONE => Err(SourceError::NotFound { path: url }),
        // A server that ignores `Range` sends the whole file.
        StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
            let mut bytes = bytes;
            if let Some(range) = range.filter(|_| status == StatusCode::OK) {
                let total = bytes.len();
                let clamp = |at: u64| usize::try_from(at).map_or(total, |at| at.min(total));
                bytes.truncate(clamp(range.end));
                bytes.drain(..clamp(range.start));
                return Ok(Fetched {
                    bytes,
                    total: Some(total as u64),
                });
            }
            Ok(Fetched { bytes, total })
        }
        // Past the end: a range the file does not reach holds nothing.
        StatusCode::RANGE_NOT_SATISFIABLE if range.is_some() => Ok(Fetched {
            bytes: Vec::new(),
            total,
        }),
        status => Err(network(format!("the server answered {status}"))),
    }
}

async fn fetch(
    url: &str,
    range: Option<Range<u64>>,
) -> Result<(StatusCode, Vec<u8>, Option<u64>), String> {
    let client = CLIENT.as_ref().map_err(Clone::clone)?;
    let mut request = client.get(url);
    if let Some(range) = &range {
        request = request.header(RANGE, format!("bytes={}-{}", range.start, range.end - 1));
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
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
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    Ok((status, bytes.to_vec(), total))
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
    let pin = match use_pin {
        Some(pin) => Some((pin, PinnedBy::Use)),
        None => lock_root
            .map(|root| locked_pin(root, url, extent))
            .transpose()?
            .flatten()
            .map(|pin| (pin, PinnedBy::Lock)),
    };
    if let Some((pin, _)) = pin
        && let Some(bytes) = cached(pin)
    {
        return Ok(bytes);
    }
    let bytes = match extent {
        Some(0) => Vec::new(),
        Some(extent) => get(url.to_string(), Some(0..extent)).await?.bytes,
        None => get(url.to_string(), None).await?.bytes,
    };
    let actual = Integrity::of(&bytes);
    match (pin, lock_root) {
        (Some((pin, pinned_by)), _) => check_digest(url, actual, pin, pinned_by)?,
        (None, Some(root)) => record_pin(root, url, extent, actual)?,
        (None, None) => {
            return Err(SourceError::Unpinned {
                url: url.to_string(),
                actual,
            });
        }
    }
    store(actual, &bytes);
    Ok(bytes)
}

fn lock_error(root: &Path, message: String) -> SourceError {
    SourceError::IoError {
        path: root.join(LOCK_FILENAME).display().to_string(),
        message,
    }
}

fn read_lock(root: &Path) -> Result<Option<LockFile>, SourceError> {
    let Ok(text) = std::fs::read_to_string(root.join(LOCK_FILENAME)) else {
        return Ok(None);
    };
    text.parse()
        .map(Some)
        .map_err(|e: LockFileError| lock_error(root, e.to_string()))
}

/// What `lock`, the lock of the package at `root`, pins `url`'s first
/// `extent` bytes to.
fn pin_in(
    lock: &LockFile,
    root: &Path,
    url: &str,
    extent: Option<u64>,
) -> Result<Option<Integrity>, SourceError> {
    let Some(text) = lock.remote_pin(url, extent) else {
        return Ok(None);
    };
    Integrity::parse(text).map(Some).ok_or_else(|| {
        lock_error(
            root,
            format!("the pin of '{url}' is not {}", Integrity::FORM),
        )
    })
}

fn locked_pin(
    root: &Path,
    url: &str,
    extent: Option<u64>,
) -> Result<Option<Integrity>, SourceError> {
    match read_lock(root)? {
        Some(lock) => pin_in(&lock, root, url, extent),
        None => Ok(None),
    }
}

/// Record `actual` as the pin of `url`'s first `extent` bytes, unless another
/// read recorded one first, which `actual` is then held to.
fn record_pin(
    root: &Path,
    url: &str,
    extent: Option<u64>,
    actual: Integrity,
) -> Result<(), SourceError> {
    let _held = lock(&LOCK_WRITE);
    let mut lock_file = match read_lock(root)? {
        Some(lock_file) => {
            if let Some(pin) = pin_in(&lock_file, root, url, extent)? {
                return check_digest(url, actual, pin, PinnedBy::Lock);
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

/// The bytes kept for `pin`, when they still hash to it.
fn cached(pin: Integrity) -> Option<Vec<u8>> {
    let bytes = std::fs::read(blob_path(pin)?).ok()?;
    (Integrity::of(&bytes) == pin).then_some(bytes)
}

/// Keep `bytes` for the next read pinned to them. A cache that cannot be
/// written costs only a fetch.
fn store(integrity: Integrity, bytes: &[u8]) {
    if let Some(path) = blob_path(integrity) {
        let _ = write_atomic(&path, bytes);
    }
}

/// A remote file read a range at a time, as a Kiln probe reads one: each
/// [`BLOCK`] it touches is fetched once.
pub struct RemoteReader {
    url: String,
    len: u64,
    blocks: Mutex<IndexMap<u64, Arc<Vec<u8>>>>,
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
        let reader = Self {
            url: url.to_string(),
            len,
            blocks: Mutex::default(),
        };
        lock(&reader.blocks).insert(0, Arc::new(first.bytes));
        Ok(reader)
    }

    fn block(&self, index: u64) -> Result<Arc<Vec<u8>>, SourceError> {
        if let Some(block) = lock(&self.blocks).get(&index) {
            return Ok(Arc::clone(block));
        }
        let start = index * BLOCK;
        let fetched =
            futures::executor::block_on(get(self.url.clone(), Some(start..start + BLOCK)))?;
        let block = Arc::new(fetched.bytes);
        lock(&self.blocks).insert(index, Arc::clone(&block));
        Ok(block)
    }
}

impl SourceReader for RemoteReader {
    fn byte_len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, start: u64, buf: &mut [u8]) -> Result<usize, SourceError> {
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

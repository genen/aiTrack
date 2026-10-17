// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only access to the Claude desktop app's Chromium HTTP cache, to find the last response of
//! `https://claude.ai/api/organizations/<org>/usage` (fresh percentages *and* reset times).
//!
//! Two on-disk layouts are understood, both documented in Chromium's `net/disk_cache`:
//!
//! * **blockfile** (what Electron apps use): `index`, `data_N` block files and `f_NNNNNN` files.
//!   Entries are 256-byte records in the block files; stream 0 holds the response headers
//!   (a pickle), stream 1 the body.
//! * **simple cache**: one `<16 hex>_0` file per entry (header, key, body, headers, trailers).
//!
//! Nothing here writes, deletes or locks anything; files are opened read-only (retrying once if the
//! app has one locked) and size-capped. Every surprise becomes a descriptive error, never a panic.
//! Only the usage entry's numbers are ever used; cookies and other entries are not looked at.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Largest cache data file read whole (block files with entries).
const MAX_BLOCK_FILE: u64 = 64 * 1024 * 1024;
/// Largest single stream (headers or body) read from a cache entry.
const MAX_STREAM: usize = 4 * 1024 * 1024;
/// Largest decompressed body.
const MAX_BODY: u64 = 4 * 1024 * 1024;
const BLOCK_HEADER: usize = 8192;
const ENTRY_SIZE: usize = 256;
const BLOCK_MAGIC: u32 = 0xC104CAC3;
const SIMPLE_HEADER_MAGIC: u64 = 0xfcfb6d1ba7725c30;
const SIMPLE_EOF_MAGIC: u64 = 0xf4fa6f45970d41d8;
const USAGE_PREFIX: &str = "https://claude.ai/api/organizations/";

/// Which on-disk layout an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Blockfile,
    Simple,
}

impl Storage {
    pub fn label(self) -> &'static str {
        match self {
            Storage::Blockfile => "blockfile",
            Storage::Simple => "simple cache",
        }
    }
}

/// One cached `.../organizations/<org>/usage` response.
#[derive(Debug, Clone)]
pub struct UsageEntry {
    /// Organisation id from the URL. Used only to keep organisations apart; never print it.
    pub org: String,
    /// When the response was received: the later of its `Date` header and (blockfile) the entry's
    /// own creation time, else the file's modification time. Only per-entry times count: a block
    /// file is shared by many entries, so its mtime says nothing about this one, and a simple-cache
    /// file's mtime is its *last use*, which a mere read of an old response would refresh.
    pub observed: Option<DateTime<Utc>>,
    pub encoding: Option<String>,
    /// The decoded JSON body, or why it could not be read.
    pub body: Result<Value, String>,
    pub storage: Storage,
    /// The cache key with the organisation id masked (`.../organizations/<org>/usage`).
    pub key_shape: String,
    /// The response's `Date` header.
    pub date_header: Option<DateTime<Utc>>,
    /// The blockfile entry's creation time (when Chromium stored it).
    pub created: Option<DateTime<Utc>>,
    /// Modification time of the file holding the entry (a whole block file, or this simple file).
    pub file_time: Option<DateTime<Utc>>,
    /// Name of that file (`data_1`, `1a2b..._0`): structure, not content.
    pub file: String,
}

#[derive(Debug, Clone, Default)]
pub struct CacheScan {
    pub dir_exists: bool,
    /// Regular files in the directory.
    pub files: usize,
    pub entries: Vec<UsageEntry>,
    /// Problems worth showing (locked files, unreadable block files).
    pub notes: Vec<String>,
}

// ---------------------------------------------------------------- reading files safely

fn is_lock_error(e: &std::io::Error) -> bool {
    // ERROR_SHARING_VIOLATION (32) / ERROR_LOCK_VIOLATION (33) on Windows
    e.kind() == std::io::ErrorKind::PermissionDenied
        || matches!(e.raw_os_error(), Some(32) | Some(33))
}

fn describe_io(e: &std::io::Error) -> String {
    if is_lock_error(e) {
        "file is locked or not readable (is the Claude app writing it?)".into()
    } else {
        e.to_string()
    }
}

/// Run `f`; if it fails with a lock/permission error, wait briefly and try once more.
fn retry_once<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    match f() {
        Err(e) if is_lock_error(&e) => {
            std::thread::sleep(std::time::Duration::from_millis(80));
            f()
        }
        other => other,
    }
}

fn read_whole(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    retry_once(|| {
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        if len > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("file too large ({len} bytes)"),
            ));
        }
        let mut buf = Vec::with_capacity(len as usize);
        f.take(cap).read_to_end(&mut buf)?;
        Ok(buf)
    })
    .map_err(|e| describe_io(&e))
}

fn read_range(path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 || len > MAX_STREAM {
        return Err("stream length out of range".into());
    }
    retry_once(|| {
        let mut f = std::fs::File::open(path)?;
        f.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        f.read_exact(&mut buf)?;
        Ok(buf)
    })
    .map_err(|e| describe_io(&e))
}

fn mtime(path: &Path) -> Option<DateTime<Utc>> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
}

// ---------------------------------------------------------------- byte helpers

/// Chromium's `base::Time` internal value: microseconds since 1601-01-01 UTC. Anything outside a
/// believable range (never set, garbage) is `None`.
fn filetime_micros(v: i64, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let secs = v / 1_000_000 - 11_644_473_600;
    let t = DateTime::from_timestamp(secs, 0)?;
    let lo = DateTime::from_timestamp(1_420_070_400, 0)?; // 2015-01-01
    (t >= lo && t <= now + chrono::Duration::days(1)).then_some(t)
}

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn i64_at(b: &[u8], o: usize) -> Option<i64> {
    u64_at(b, o).map(|v| v as i64)
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

// ---------------------------------------------------------------- key and header parsing

/// If `key` names a usage URL, the organisation id. Chromium may put prefixes (upload ids, a
/// network-isolation key) before the URL and a query string after it.
pub fn usage_org_from_key(key: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(key);
    let start = text.rfind("https://")?;
    let url = text[start..].split(['?', '#', '\0']).next()?;
    let rest = url.strip_prefix(USAGE_PREFIX)?;
    let org = rest.strip_suffix("/usage")?;
    (!org.is_empty() && !org.contains('/')).then(|| org.to_owned())
}

#[derive(Debug, Default)]
pub struct Headers {
    pub ok_json: bool,
    pub truncated: bool,
    pub encoding: Option<String>,
    pub date: Option<DateTime<Utc>>,
}

/// Parse the stored response info (a Chromium pickle). The header block is found by its length-
/// prefixed `HTTP/…` status line instead of fixed offsets, so optional fields before it do not matter.
pub fn parse_response_info(meta: &[u8]) -> Result<Headers, String> {
    let mut at = 0;
    let block = loop {
        let p = meta[at..]
            .windows(5)
            .position(|w| w == b"HTTP/")
            .map(|i| i + at)
            .ok_or("no HTTP headers in the cache entry")?;
        if p >= 4 {
            let len = u32_at(meta, p - 4).unwrap_or(0) as usize;
            if (12..=64 * 1024).contains(&len) && p + len <= meta.len() {
                break &meta[p..p + len];
            }
        }
        at = p + 1;
    };
    let text = String::from_utf8_lossy(block);
    let mut lines = text.split('\0').filter(|l| !l.is_empty());
    let status = lines.next().unwrap_or("");
    let status_ok = status.split_whitespace().nth(1) == Some("200");
    let mut h = Headers::default();
    let mut json = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        match k.as_str() {
            "content-type" => json = v.to_ascii_lowercase().contains("application/json"),
            "content-encoding" => h.encoding = Some(v.to_ascii_lowercase()),
            "date" => {
                h.date = DateTime::parse_from_rfc2822(v)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            }
            _ => {}
        }
    }
    h.ok_json = status_ok && json;
    // flags (right after the pickle's length word) carry a "truncated" bit for version-3 infos
    if let (Some(flags), Some(true)) = (
        u32_at(meta, 4),
        u32_at(meta, 0).map(|n| n as usize == meta.len() - 4),
    ) {
        h.truncated = flags & 0xFF == 3 && flags & (1 << 12) != 0;
    }
    Ok(h)
}

// ---------------------------------------------------------------- body decoding

/// Decode a stored body. The on-disk frame is detected by its magic first (Chromium can keep a
/// zstd frame whatever the original encoding was), then the `Content-Encoding` header.
pub fn decode_body(raw: &[u8], encoding: Option<&str>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let enc = encoding.unwrap_or("");
    if raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        let mut d =
            ruzstd::decoding::StreamingDecoder::new_with_max_window_size(raw, 8 * 1024 * 1024)
                .map_err(|e| format!("invalid zstd body: {e}"))?;
        (&mut d)
            .take(MAX_BODY + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("invalid zstd body: {e}"))?;
    } else if raw.starts_with(&[0x1F, 0x8B]) {
        flate2::read::GzDecoder::new(raw)
            .take(MAX_BODY + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("invalid gzip body: {e}"))?;
    } else if enc.contains("deflate") {
        flate2::read::ZlibDecoder::new(raw)
            .take(MAX_BODY + 1)
            .read_to_end(&mut out)
            .map_err(|e| format!("invalid deflate body: {e}"))?;
    } else if enc.contains("br") {
        return Err(
            "body is brotli-compressed (content-encoding: br), which is not supported".into(),
        );
    } else {
        out.extend_from_slice(raw);
    }
    if out.len() as u64 > MAX_BODY {
        return Err("response body is too large".into());
    }
    Ok(out)
}

/// Where an entry was found (everything `finish` needs besides the response itself).
struct Found {
    storage: Storage,
    key_shape: String,
    created: Option<DateTime<Utc>>,
    file_time: Option<DateTime<Utc>>,
    file: String,
}

impl Found {
    fn entry(
        self,
        org: String,
        observed: Option<DateTime<Utc>>,
        date_header: Option<DateTime<Utc>>,
        encoding: Option<String>,
        body: Result<Value, String>,
    ) -> UsageEntry {
        UsageEntry {
            org,
            observed,
            encoding,
            body,
            storage: self.storage,
            key_shape: self.key_shape,
            date_header,
            created: self.created,
            file_time: self.file_time,
            file: self.file,
        }
    }
}

/// The key with the organisation id masked, for diagnostics.
fn mask_key(key: &[u8], org: &str) -> String {
    let text = String::from_utf8_lossy(key);
    let masked = if org.is_empty() {
        text.into_owned()
    } else {
        text.replace(org, "<org>")
    };
    masked.chars().take(160).collect()
}

fn fnv64(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in parts {
        for b in *p {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

type DecodeMemo = std::sync::Mutex<std::collections::HashMap<u64, Result<Value, String>>>;

thread_local! {
    /// Bodies this thread really decoded (memo misses); the scans run on the calling thread.
    static DECODED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many bodies scans on the calling thread have decoded for real so far (memo misses).
/// Tests use it to check that repeated scans of unchanged files do no decoding.
pub fn decodes_on_this_thread() -> u64 {
    DECODED.with(|d| d.get())
}

/// Entries kept; once full the memo starts over. Large enough that a process with several
/// caches or parallel tests does not evict the bodies of a scan that is still in progress.
const MEMO_MAX: usize = 1024;

fn decode_memo() -> &'static DecodeMemo {
    static MEMO: std::sync::OnceLock<DecodeMemo> = std::sync::OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// Decompress and parse a body, remembering the result by a hash of the raw bytes (and the
/// encoding): the files are read afresh on every scan, but the same unchanged response is only
/// decoded once. A different body can never hit the memo, whatever mtime or size the file shows.
fn decode_json(raw: &[u8], encoding: Option<&str>) -> Result<Value, String> {
    let key = fnv64(&[
        raw,
        encoding.unwrap_or("").as_bytes(),
        &(raw.len() as u64).to_le_bytes(),
    ]);
    if let Some(hit) = decode_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return hit.clone();
    }
    DECODED.with(|d| d.set(d.get() + 1));
    let parsed =
        decode_body(raw, encoding).and_then(|raw| match serde_json::from_slice::<Value>(&raw) {
            Ok(v) if v.is_object() => Ok(v),
            Ok(_) => Err("usage body is not a JSON object".to_string()),
            Err(_) => Err("usage body is not valid JSON".to_string()),
        });
    let mut m = decode_memo().lock().unwrap_or_else(|e| e.into_inner());
    if m.len() >= MEMO_MAX {
        m.clear();
    }
    m.insert(key, parsed.clone());
    parsed
}

fn finish(
    org: String,
    headers: Result<Headers, String>,
    body: Result<Vec<u8>, String>,
    found: Found,
) -> Option<UsageEntry> {
    match headers {
        Err(e) => {
            let t = found.created.or(found.file_time);
            Some(found.entry(org, t, None, None, Err(e)))
        }
        Ok(h) if h.truncated => None, // an in-flight or incomplete write
        Ok(h) if !h.ok_json => None,  // an error page or something else that is not usage JSON
        Ok(h) => {
            let observed = match (h.date, found.created) {
                (Some(d), Some(c)) => Some(d.max(c)),
                (d, c) => d.or(c).or(found.file_time),
            };
            let parsed = body.and_then(|raw| decode_json(&raw, h.encoding.as_deref()));
            Some(found.entry(org, observed, h.date, h.encoding, parsed))
        }
    }
}

// ---------------------------------------------------------------- blockfile

fn block_allocated(data: &[u8], index: usize) -> bool {
    // the allocation bitmap follows the 80-byte block-file header fields
    u32_at(data, 80 + (index / 32) * 4).is_some_and(|w| w & (1 << (index % 32)) != 0)
}

/// Read one cache stream by its Chromium address.
fn read_stream(dir: &Path, addr: u32, len: i32) -> Result<Vec<u8>, String> {
    if addr & 0x8000_0000 == 0 || len <= 0 {
        return Err("invalid cache address".into());
    }
    let len = len as usize;
    match (addr >> 28) & 7 {
        0 => read_range(&dir.join(format!("f_{:06x}", addr & 0x0FFF_FFFF)), 0, len),
        kind @ 2..=4 => {
            let size = [256usize, 1024, 4096][kind as usize - 2];
            let blocks = ((addr >> 24) & 3) as usize + 1;
            if len > size * blocks {
                return Err("stream longer than its allocation".into());
            }
            let file = (addr >> 16) & 0xFF;
            let block = (addr & 0xFFFF) as usize;
            read_range(
                &dir.join(format!("data_{file}")),
                (BLOCK_HEADER + block * size) as u64,
                len,
            )
        }
        _ => Err("unsupported cache address type".into()),
    }
}

fn scan_block_file(
    dir: &Path,
    name: &str,
    data: &[u8],
    file_time: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    out: &mut Vec<UsageEntry>,
) {
    if data.len() < BLOCK_HEADER
        || u32_at(data, 0) != Some(BLOCK_MAGIC)
        || i32_at(data, 12) != Some(ENTRY_SIZE as i32)
    {
        return; // not a file of 256-byte entries (rankings, 1K/4K stream files, ...)
    }
    let blocks = (data.len() - BLOCK_HEADER) / ENTRY_SIZE;
    for i in 0..blocks {
        if !block_allocated(data, i) {
            continue;
        }
        let off = BLOCK_HEADER + i * ENTRY_SIZE;
        let entry = &data[off..off + ENTRY_SIZE];
        let Some(e) = read_entry(dir, name, off, entry, file_time, now) else {
            continue;
        };
        out.push(e);
    }
}

fn read_entry(
    dir: &Path,
    file_name: &str,
    off: usize,
    entry: &[u8],
    file_time: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<UsageEntry> {
    if i32_at(entry, 20)? != 0 {
        return None; // state: not a normal, completed entry
    }
    let key_len = i32_at(entry, 32)?;
    if !(1..=4096).contains(&key_len) {
        return None;
    }
    let long_key = u32_at(entry, 36)?;
    let key = if long_key != 0 {
        read_stream(dir, long_key, key_len).ok()?
    } else {
        entry.get(96..96 + usize::try_from(key_len).ok()?)?.to_vec()
    };
    let org = usage_org_from_key(&key)?;
    let created = i64_at(entry, 24).and_then(|v| filetime_micros(v, now));
    let (meta_len, body_len) = (i32_at(entry, 40)?, i32_at(entry, 44)?);
    let (meta_addr, body_addr) = (u32_at(entry, 56)?, u32_at(entry, 60)?);
    let meta = read_stream(dir, meta_addr, meta_len);
    let body = read_stream(dir, body_addr, body_len);
    // The app may be rewriting the entry right now: trust it only if it reads back unchanged.
    let again = read_range(&dir.join(file_name), off as u64, ENTRY_SIZE).ok();
    if again.as_deref() != Some(entry) {
        return None;
    }
    if let (Ok(m), Ok(m2)) = (&meta, read_stream(dir, meta_addr, meta_len)) {
        if *m != m2 {
            return None;
        }
    }
    let found = Found {
        storage: Storage::Blockfile,
        key_shape: mask_key(&key, &org),
        created,
        file_time,
        file: file_name.to_owned(),
    };
    finish(org, meta.and_then(|m| parse_response_info(&m)), body, found)
}

// ---------------------------------------------------------------- simple cache

fn is_simple_name(name: &str) -> bool {
    name.len() == 18 && name.ends_with("_0") && name[..16].bytes().all(|b| b.is_ascii_hexdigit())
}

fn read_simple(data: &[u8], file_time: Option<DateTime<Utc>>, file: &str) -> Option<UsageEntry> {
    if u64_at(data, 0)? != SIMPLE_HEADER_MAGIC {
        return None;
    }
    let key_len = u32_at(data, 12)? as usize;
    let key = data.get(24..24 + key_len)?;
    let org = usage_org_from_key(key)?;
    let found = Found {
        storage: Storage::Simple,
        key_shape: mask_key(key, &org),
        created: None,
        file_time,
        file: file.to_owned(),
    };
    // layout: header, key, body, EOF(body), headers, [key sha256], EOF(headers)
    let end = data.len();
    let eof = end.checked_sub(24)?;
    if u64_at(data, eof)? != SIMPLE_EOF_MAGIC {
        return Some(found.entry(
            org,
            file_time,
            None,
            None,
            Err("incomplete simple-cache entry".into()),
        ));
    }
    let (flags, meta_len) = (u32_at(data, eof + 8)?, u32_at(data, eof + 16)? as usize);
    let meta_end = eof.checked_sub(if flags & 2 != 0 { 32 } else { 0 })?;
    let meta_start = meta_end.checked_sub(meta_len)?;
    let body_eof = meta_start.checked_sub(24)?;
    let meta = data.get(meta_start..meta_end)?;
    let layout_ok = u64_at(data, body_eof) == Some(SIMPLE_EOF_MAGIC) && body_eof >= 24 + key_len;
    let body = if layout_ok {
        Ok(data[24 + key_len..body_eof].to_vec())
    } else {
        Err("unrecognised simple-cache layout".to_string())
    };
    finish(org, parse_response_info(meta), body, found)
}

// ---------------------------------------------------------------- directory scan

/// Find every usage response in the cache directory (`…/Cache/Cache_Data`), at the real time.
pub fn scan_usage_entries(dir: &Path) -> CacheScan {
    scan_usage_entries_at(dir, Utc::now())
}

/// [`scan_usage_entries`] as seen at `now` (an entry creation time more than a day after `now` is
/// not believable and is ignored).
pub fn scan_usage_entries_at(dir: &Path, now: DateTime<Utc>) -> CacheScan {
    let mut scan = CacheScan {
        dir_exists: dir.is_dir(),
        ..Default::default()
    };
    if !scan.dir_exists {
        return scan;
    }
    let names: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(Result::ok)
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(e) => {
            scan.notes.push(format!(
                "cache directory is not readable: {}",
                describe_io(&e)
            ));
            return scan;
        }
    };
    scan.files = names.len();
    for name in &names {
        let path = dir.join(name);
        if name
            .strip_prefix("data_")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            match read_whole(&path, MAX_BLOCK_FILE) {
                Ok(data) => scan_block_file(dir, name, &data, mtime(&path), now, &mut scan.entries),
                Err(e) => scan.notes.push(format!("{name}: {e}")),
            }
        } else if is_simple_name(name) {
            match read_whole(&path, MAX_STREAM as u64 * 2) {
                Ok(data) => scan.entries.extend(read_simple(&data, mtime(&path), name)),
                Err(e) => scan.notes.push(format!("{name}: {e}")),
            }
        }
    }
    scan
}

// ---------------------------------------------------------------- repeated scans

/// [`scan_usage_entries`] for repeated polling. There is deliberately **no** shortcut based on
/// file names, sizes or modification times: Chromium rewrites `data_N` / `f_*` files in place
/// through memory mappings, which on Windows can leave size *and* mtime untouched for a long
/// time, so a stat-only fingerprint kept serving an old reading while the app's cache already held
/// a newer one. Every call reads the files (a few tens of milliseconds for typical caches, see the
/// timing test) and only the expensive step, decompression plus JSON parsing, is memoised, keyed
/// by a hash of the response's raw bytes, so it can never return a body that changed.
pub fn scan_usage_entries_cached(dir: &Path) -> CacheScan {
    scan_usage_entries(dir)
}

/// [`scan_usage_entries_cached`] as seen at `now`.
pub fn scan_usage_entries_cached_at(dir: &Path, now: DateTime<Utc>) -> CacheScan {
    scan_usage_entries_at(dir, now)
}

// ---------------------------------------------------------------- choosing between entries

/// Entries whose timestamps differ by less than this are treated as simultaneous.
pub const TIE_SECS: i64 = 30;

/// The chosen entry and why.
#[derive(Debug, Clone)]
pub struct Pick<'a> {
    pub entry: &'a UsageEntry,
    pub reason: String,
}

fn window(body: &Result<Value, String>, key: &str) -> Option<(f64, Option<String>)> {
    let b = body.as_ref().ok()?.get(key)?;
    Some((
        b.get("utilization")?.as_f64()?,
        b.get("resets_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
    ))
}

/// Pick the entry that shows the newest information: the newest `observed` time; entries within
/// [`TIE_SECS`] of it count as simultaneous, and among those (same window, same `resets_at`)
/// the higher utilization wins, because usage only grows inside a window. A usable body beats an
/// unusable one in a tie.
pub fn pick_best<'a>(entries: impl IntoIterator<Item = &'a UsageEntry>) -> Option<Pick<'a>> {
    let all: Vec<&UsageEntry> = entries.into_iter().collect();
    let newest = all.iter().filter_map(|e| e.observed).max();
    let tied: Vec<&UsageEntry> = all
        .iter()
        .copied()
        .filter(|e| match (e.observed, newest) {
            (Some(t), Some(n)) => (n - t).num_seconds() <= TIE_SECS,
            (None, None) => true,
            _ => false,
        })
        .collect();
    let first = *tied.first()?;
    if all.len() == 1 {
        return Some(Pick {
            entry: first,
            reason: "the only matching entry".into(),
        });
    }
    if tied.len() == 1 {
        return Some(Pick {
            entry: first,
            reason: "newest timestamp".into(),
        });
    }
    // among simultaneous entries: usable first, then higher five_hour, then higher seven_day
    let score = |e: &UsageEntry| {
        let f = window(&e.body, "five_hour");
        let s = window(&e.body, "seven_day");
        (
            e.body.is_ok(),
            f.as_ref().map(|w| w.0),
            s.as_ref().map(|w| w.0),
            e.observed,
        )
    };
    let mut best = first;
    for e in tied.iter().skip(1) {
        let (a, b) = (score(e), score(best));
        let same_window = window(&e.body, "five_hour").map(|w| w.1)
            == window(&best.body, "five_hour").map(|w| w.1);
        let better = if a.0 != b.0 {
            a.0
        } else if same_window {
            (a.1, a.2, a.3) > (b.1, b.2, b.3)
        } else {
            a.3 > b.3
        };
        if better {
            best = e;
        }
    }
    Some(Pick {
        entry: best,
        reason: format!(
            "{} entries within {TIE_SECS} s of the newest timestamp: higher utilization in the same window",
            tied.len()
        ),
    })
}

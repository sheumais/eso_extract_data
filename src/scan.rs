//! Bulk text search across an entire Elder Scrolls Online install (140 GB+).
//!
//! Every `*.mnf` under the root is loaded with [`eso_common::mnf::MnfFile`]; each sub-file in its
//! `*NNNN.dat` archives is read and decompressed (zlib / snappy / Oodle, whatever the entry uses)
//! and searched in memory. Two details matter for getting real hits:
//!
//! * `EsoFileData` sub-files are containers of zlib-compressed records, so the text is invisible
//!   until each record is inflated. Those records are inflated and searched too.
//! * With "match all" (the default) the patterns only have to occur somewhere in the *same
//!   sub-file*, not in the same record, because the structure may split them across records.
//!
//! Loose (non-archive) files in the folder are searched as raw bytes in the same run.
//!
//! The work is spread over a thread pool: each MNF's entries are sorted by archive/offset (so
//! reads stay sequential) and cut into chunks that the workers pull from a shared queue.
//!
//! Library logging is switched off while scanning (see [`run`]); problems are counted in the
//! [`ScanSummary`] instead of being printed per entry.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use eso_common::dat::DatSubFileInfo;
use eso_common::file::CFile;
use eso_common::mnf::{MnfFile, MnfFileEntry};
use eso_common::oodle::load_oodle_lib_from;
use eso_common::util::inflate_zlib_block;

const LOOSE_CHUNK: usize = 8 << 20;
const CHUNK_MAX_ENTRIES: usize = 2048;
const CHUNK_MAX_BYTES: u64 = 256 << 20;
/// Records claiming to inflate to more than this are treated as corrupt.
const MAX_RECORD_BYTES: usize = 1 << 29;
const MAX_ERROR_SAMPLES: usize = 40;

// ---------------------------------------------------------------------------------------------
// Options / results
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Folder searched recursively for `*.mnf` and loose files.
    pub root: PathBuf,
    pub patterns: Vec<String>,
    /// `true`: all patterns must occur in the same sub-file/file. `false`: any one pattern is enough.
    pub match_all: bool,
    pub case_sensitive: bool,
    /// Also look for the UTF-16LE form of every pattern.
    pub utf16: bool,
    /// Search loose files in the folder (everything except `*.mnf` and the `.dat` archives of an MNF).
    pub scan_loose: bool,
    /// Inflate and search the records inside `EsoFileData` sub-files.
    pub scan_records: bool,
    /// Worker threads; 0 = one per logical CPU.
    pub threads: usize,
    /// Oodle library file or folder; the usual locations are tried as well.
    pub oodle_path: Option<String>,
    /// Only scan MNF files whose path contains this text (case-insensitive).
    pub mnf_filter: Option<String>,
    /// Write one CSV row per hit to this file.
    pub csv_path: Option<PathBuf>,
    /// Bytes of context shown on each side of a match.
    pub context_bytes: usize,
    /// Seconds between progress lines on stderr; 0 = none.
    pub progress_secs: u64,
    /// No hit/progress output at all (the [`ScanSummary`] still has everything).
    pub quiet: bool,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            patterns: Vec::new(),
            match_all: true,
            case_sensitive: false,
            utf16: false,
            scan_loose: true,
            scan_records: true,
            threads: 0,
            oodle_path: None,
            mnf_filter: None,
            csv_path: None,
            context_bytes: 24,
            progress_secs: 10,
            quiet: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// ASCII / UTF-8 bytes.
    Utf8,
    Utf16Le,
}

impl Encoding {
    fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf8",
            Encoding::Utf16Le => "utf16le",
        }
    }
}

/// Where in the archives a hit lives. `None` for loose files.
#[derive(Debug, Clone)]
pub struct EntryRef {
    pub archive: u8,
    /// Absolute index in the MNF file table.
    pub index: u32,
    /// The real file index (what `EsoExtractData -f` expects).
    pub file_index: u32,
    /// Name from the ZOSFT, empty if the entry has none.
    pub name: String,
    /// Decompressed size of the sub-file.
    pub size: usize,
}

#[derive(Debug, Clone)]
pub struct PatternHit {
    pub pattern: String,
    pub encoding: Encoding,
    /// 1-based record number inside an `EsoFileData` sub-file.
    pub record: Option<usize>,
    /// Byte offset of the first match inside the record / sub-file / file.
    pub offset: usize,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct Hit {
    /// The MNF file, or the loose file itself.
    pub container: PathBuf,
    pub entry: Option<EntryRef>,
    pub matches: Vec<PatternHit>,
}

#[derive(Debug, Clone, Default)]
pub struct ScanSummary {
    pub oodle_loaded: bool,
    pub mnf_files: u64,
    pub mnf_load_errors: Vec<(PathBuf, String)>,
    pub entries: u64,
    pub bytes_read: u64,
    pub bytes_scanned: u64,
    pub records: u64,
    pub loose_files: u64,
    pub loose_bytes: u64,
    pub loose_errors: u64,
    pub read_errors: u64,
    /// Entries that are Oodle-compressed but could not be decompressed (library missing/failed).
    pub oodle_errors: u64,
    pub record_errors: u64,
    pub missing_dats: Vec<PathBuf>,
    pub error_samples: Vec<String>,
    pub hits: Vec<Hit>,
    pub elapsed: Duration,
}

// ---------------------------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Loc {
    record: Option<usize>,
    offset: usize,
    snippet: String,
}

/// First location of each pattern seen so far (per Aho-Corasick pattern id).
#[derive(Debug, Clone)]
pub struct Found {
    locs: Vec<Option<Loc>>,
    seen: Vec<bool>,
    n_seen: usize,
}

pub struct Needles {
    ac: AhoCorasick,
    /// Aho-Corasick pattern id -> (index into `patterns`, encoding)
    ids: Vec<(usize, Encoding)>,
    lens: Vec<usize>,
    patterns: Vec<String>,
    max_len: usize,
    match_all: bool,
    context: usize,
}

fn make_snippet(data: &[u8], start: usize, plen: usize, ctx: usize) -> String {
    let a = start.saturating_sub(ctx);
    let b = start.saturating_add(plen).saturating_add(ctx).min(data.len());
    data[a..b].iter().map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' }).collect()
}

impl Needles {
    pub fn new(patterns: &[String], case_sensitive: bool, utf16: bool, match_all: bool, context: usize) -> Result<Self, String> {
        if patterns.is_empty() {
            return Err("no search strings given".into());
        }
        let mut bytes: Vec<Vec<u8>> = Vec::new();
        let mut ids = Vec::new();
        for (i, p) in patterns.iter().enumerate() {
            if p.is_empty() {
                return Err("search strings must not be empty".into());
            }
            bytes.push(p.as_bytes().to_vec());
            ids.push((i, Encoding::Utf8));
            if utf16 {
                bytes.push(p.encode_utf16().flat_map(|u| u.to_le_bytes()).collect());
                ids.push((i, Encoding::Utf16Le));
            }
        }
        let lens: Vec<usize> = bytes.iter().map(Vec::len).collect();
        let max_len = lens.iter().copied().max().unwrap_or(1);
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::Standard) // needed so overlapping matches (a prefix of another pattern) are all reported
            .ascii_case_insensitive(!case_sensitive)
            .kind(Some(AhoCorasickKind::DFA))
            .build(&bytes)
            .map_err(|e| e.to_string())?;
        Ok(Self { ac, ids, lens, patterns: patterns.to_vec(), max_len, match_all, context })
    }

    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    fn new_found(&self) -> Found {
        Found { locs: vec![None; self.ids.len()], seen: vec![false; self.patterns.len()], n_seen: 0 }
    }

    fn record(&self, f: &mut Found, id: usize, record: Option<usize>, offset: usize, window: &[u8], rel: usize) {
        if f.locs[id].is_some() {
            return;
        }
        f.locs[id] = Some(Loc { record, offset, snippet: make_snippet(window, rel, self.lens[id], self.context) });
        let pi = self.ids[id].0;
        if !f.seen[pi] {
            f.seen[pi] = true;
            f.n_seen += 1;
        }
    }

    /// True when nothing more can change the verdict (all patterns seen in "all" mode).
    pub fn complete(&self, f: &Option<Found>) -> bool {
        self.match_all && f.as_ref().is_some_and(|f| f.n_seen == self.patterns.len())
    }

    /// Does `f` satisfy the any/all rule?
    pub fn satisfied(&self, f: &Option<Found>) -> bool {
        match f {
            None => false,
            Some(f) if self.match_all => f.n_seen == self.patterns.len(),
            Some(f) => f.n_seen > 0,
        }
    }

    /// Searches `data`, recording first locations into `slot` (created on the first match so the
    /// common no-match case allocates nothing). `record` is the EsoFileData record number, if any.
    pub fn scan_slice(&self, data: &[u8], record: Option<usize>, slot: &mut Option<Found>) {
        for m in self.ac.find_overlapping_iter(data) {
            let f = slot.get_or_insert_with(|| self.new_found());
            self.record(f, m.pattern().as_usize(), record, m.start(), data, m.start());
            if self.match_all && f.n_seen == self.patterns.len() {
                break;
            }
        }
    }

    /// Streams `r` in `chunk`-sized pieces (matches spanning two chunks are still found).
    /// Returns the bytes read.
    pub fn scan_reader<R: Read>(&self, mut r: R, chunk: usize, slot: &mut Option<Found>) -> io::Result<u64> {
        let keep = self.max_len.saturating_sub(1);
        let chunk = chunk.max(1);
        let mut buf = vec![0u8; chunk + keep];
        let mut carry = 0usize; // bytes at the start of `buf` that were already searched
        let mut base = 0usize; // absolute offset of buf[0]
        let mut total = 0u64;
        loop {
            let mut filled = carry;
            while filled < carry + chunk {
                let n = r.read(&mut buf[filled..carry + chunk])?;
                if n == 0 {
                    break;
                }
                filled += n;
                total += n as u64;
            }
            if filled == carry {
                break;
            }
            for m in self.ac.find_overlapping_iter(&buf[..filled]) {
                if m.end() > carry {
                    // not entirely inside the part that the previous pass already covered
                    let f = slot.get_or_insert_with(|| self.new_found());
                    self.record(f, m.pattern().as_usize(), None, base + m.start(), &buf[..filled], m.start());
                }
            }
            if self.complete(slot) || filled < carry + chunk {
                break;
            }
            let new_carry = keep.min(filled);
            buf.copy_within(filled - new_carry..filled, 0);
            base += filled - new_carry;
            carry = new_carry;
        }
        Ok(total)
    }

    fn matches_of(&self, f: &Found) -> Vec<PatternHit> {
        let mut v: Vec<PatternHit> = f
            .locs
            .iter()
            .enumerate()
            .filter_map(|(id, l)| {
                let l = l.as_ref()?;
                let (pi, enc) = self.ids[id];
                Some(PatternHit { pattern: self.patterns[pi].clone(), encoding: enc, record: l.record, offset: l.offset, snippet: l.snippet.clone() })
            })
            .collect();
        v.sort_by_key(|m| (m.record.unwrap_or(0), m.offset));
        v
    }
}

// ---------------------------------------------------------------------------------------------
// EsoFileData records
// ---------------------------------------------------------------------------------------------

const ESO_FILE_DATA_MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];

fn be32(d: &[u8], off: usize) -> usize {
    u32::from_be_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]]) as usize
}

/// Walks the records of an `EsoFileData` sub-file (16 byte header, then records of
/// `[size1, size2, compressed size]` big-endian dwords followed by zlib data), inflating each and
/// passing `(record number starting at 1, bytes)` to `f`. `f` returns `false` to stop early.
///
/// Mirrors the stop conditions of `EsoSubFileDataFile::parse`. Records that fail to inflate are
/// counted in `bad` and skipped. Returns the number of records seen.
pub fn for_each_record(data: &[u8], buf: &mut Vec<u8>, bad: &mut u64, mut f: impl FnMut(usize, &[u8]) -> bool) -> usize {
    const HEADER: usize = 16;
    const REC_HEADER: usize = 12;
    if data.len() < HEADER || data[..4] != ESO_FILE_DATA_MAGIC {
        return 0;
    }
    let mut pos = HEADER;
    let mut n = 0usize;
    while data.len().saturating_sub(pos) >= REC_HEADER {
        let (s1, s2, cs) = (be32(data, pos), be32(data, pos + 4), be32(data, pos + 8));
        let start = pos + REC_HEADER;
        if cs == 0 || s1 == 0 || cs > data.len() - start {
            break;
        }
        n += 1;
        let cap = s1.max(s2).saturating_add(16);
        if cap > MAX_RECORD_BYTES {
            *bad += 1;
        } else {
            buf.clear();
            buf.resize(cap, 0);
            match inflate_zlib_block(&mut buf[..], &data[start..start + cs], true) {
                Ok(len) => {
                    if !f(n, &buf[..len]) {
                        return n;
                    }
                }
                Err(_) => *bad += 1,
            }
        }
        pos = start + cs;
    }
    n
}

/// The header test the library uses before attempting Oodle decompression.
fn looks_oodle_compressed(data: &[u8]) -> bool {
    data.len() >= 2 && (data[0] == 0x8C || data[0] == 0xCC) && (data[1] == 0x06 || data[1] == 0x0A)
}

// ---------------------------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Counters {
    entries: AtomicU64,
    bytes_read: AtomicU64,
    bytes_scanned: AtomicU64,
    records: AtomicU64,
    read_errors: AtomicU64,
    oodle_errors: AtomicU64,
    record_errors: AtomicU64,
    hits: AtomicU64,
    loose_files: AtomicU64,
    loose_bytes: AtomicU64,
    loose_errors: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct SinkInner {
    hits: Vec<Hit>,
    csv: Option<BufWriter<File>>,
}

struct Shared<'a> {
    opts: &'a ScanOptions,
    needles: Needles,
    counters: Counters,
    sink: Mutex<SinkInner>,
    errors: Mutex<Vec<String>>,
    missing_dats: Mutex<BTreeSet<PathBuf>>,
}

impl Shared<'_> {
    fn note_error(&self, msg: String) {
        let mut g = lock(&self.errors);
        if g.len() < MAX_ERROR_SAMPLES {
            g.push(msg);
        }
    }

    fn report(&self, hit: Hit) {
        self.counters.hits.fetch_add(1, Relaxed);
        let mut g = lock(&self.sink);
        if !self.opts.quiet {
            print_hit(&hit);
        }
        if let Some(w) = g.csv.as_mut() {
            for m in &hit.matches {
                let (arch, idx, fidx, name, size) = match &hit.entry {
                    Some(e) => (e.archive.to_string(), e.index.to_string(), e.file_index.to_string(), e.name.clone(), e.size.to_string()),
                    None => Default::default(),
                };
                let row = [
                    hit.container.to_string_lossy().into_owned(),
                    arch,
                    idx,
                    fidx,
                    name,
                    size,
                    m.record.map(|r| r.to_string()).unwrap_or_default(),
                    m.pattern.clone(),
                    m.encoding.label().to_string(),
                    m.offset.to_string(),
                ];
                let line: Vec<String> = row.iter().map(|f| format!("\"{}\"", f.replace('"', "\"\""))).collect();
                let _ = writeln!(w, "{}", line.join(","));
            }
            let _ = w.flush();
        }
        g.hits.push(hit);
    }
}

fn print_hit(hit: &Hit) {
    let mut out = String::new();
    out.push_str(&format!("HIT  {}\n", hit.container.display()));
    if let Some(e) = &hit.entry {
        out.push_str(&format!(
            "     archive {:03}, entry {} (file index {}), {} bytes{}\n",
            e.archive,
            e.index,
            e.file_index,
            e.size,
            if e.name.is_empty() { String::new() } else { format!(", {}", e.name) }
        ));
    }
    for m in &hit.matches {
        let rec = m.record.map(|r| format!("record {}, ", r)).unwrap_or_default();
        out.push_str(&format!("     {} [{}] @ {}offset 0x{:X}: \"{}\"\n", m.pattern, m.encoding.label(), rec, m.offset, m.snippet));
    }
    // One write call under the sink lock keeps hits from interleaving.
    let _ = io::stdout().write_all(out.as_bytes());
    let _ = io::stdout().flush();
}

// ---------------------------------------------------------------------------------------------
// Thread pool
// ---------------------------------------------------------------------------------------------

/// Runs `n` work items on `threads` threads. Each thread builds its own worker state with `make`
/// (so it can keep an open file and scratch buffers) and then pulls item numbers from a shared
/// counter. `progress` is printed to stderr every `interval` seconds (0 = never).
/// Returns the number of workers that panicked.
fn run_parallel<W, M>(threads: usize, n: usize, interval: u64, progress: &(dyn Fn() -> String + Sync), make: M) -> usize
where
    W: FnMut(usize),
    M: Fn() -> W + Sync,
{
    if n == 0 {
        return 0;
    }
    let next = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    let mut panics = 0;
    thread::scope(|s| {
        let handles: Vec<_> = (0..threads.max(1).min(n))
            .map(|_| {
                s.spawn(|| {
                    let mut work = make();
                    loop {
                        let i = next.fetch_add(1, Relaxed);
                        if i >= n {
                            break;
                        }
                        work(i);
                    }
                })
            })
            .collect();
        if interval > 0 {
            s.spawn(|| {
                let mut last = Instant::now();
                while !done.load(Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    if last.elapsed().as_secs() >= interval {
                        eprintln!("{}", progress());
                        last = Instant::now();
                    }
                }
            });
        }
        for h in handles {
            if h.join().is_err() {
                panics += 1;
            }
        }
        done.store(true, Relaxed);
    });
    panics
}

// ---------------------------------------------------------------------------------------------
// MNF scanning
// ---------------------------------------------------------------------------------------------

/// Cuts the (archive, offset)-sorted entry list into chunks that stay inside one archive and are
/// bounded by entry count and compressed bytes, so workers get evenly sized units of work.
pub fn plan_chunks(table: &[MnfFileEntry], order: &[usize]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0u64;
    for (k, &ti) in order.iter().enumerate() {
        let e = &table[ti];
        if k > start && (e.archive_index != table[order[start]].archive_index || k - start >= CHUNK_MAX_ENTRIES || bytes >= CHUNK_MAX_BYTES) {
            out.push(start..k);
            start = k;
            bytes = 0;
        }
        bytes += e.compressed_size as u64;
    }
    if start < order.len() {
        out.push(start..order.len());
    }
    out
}

#[derive(Default)]
struct Worker {
    input: CFile,
    open_archive: Option<u8>,
    dat_ok: bool,
    rec_buf: Vec<u8>,
}

struct MnfJob<'a> {
    sh: &'a Shared<'a>,
    mnf: &'a MnfFile,
    path: &'a Path,
    order: Vec<usize>,
    chunks: Vec<Range<usize>>,
}

impl MnfJob<'_> {
    fn process_chunk(&self, ci: usize, st: &mut Worker) {
        for &ti in &self.order[self.chunks[ci].clone()] {
            let e = self.mnf.file_table()[ti];
            self.process_entry(st, &e);
        }
    }

    fn process_entry(&self, st: &mut Worker, e: &MnfFileEntry) {
        let sh = self.sh;
        let c = &sh.counters;
        c.entries.fetch_add(1, Relaxed);
        c.bytes_read.fetch_add(e.compressed_size as u64, Relaxed);

        if st.open_archive != Some(e.archive_index) {
            st.input.close();
            st.open_archive = Some(e.archive_index);
            let name = self.mnf.create_data_filename(e.archive_index);
            st.dat_ok = st.input.open(&name, "rb").is_ok();
            if !st.dat_ok {
                lock(&sh.missing_dats).insert(PathBuf::from(name));
            }
        }
        if !st.dat_ok {
            c.read_errors.fetch_add(1, Relaxed);
            return;
        }

        let mut info = DatSubFileInfo::default();
        if self.mnf.read_data_file(e, &mut info, Some(&mut st.input)).is_err() || !info.has_file_data() {
            c.read_errors.fetch_add(1, Relaxed);
            sh.note_error(format!("{}: archive {} entry {}: could not read/decompress", self.path.display(), e.archive_index, e.index));
            return;
        }
        // Same check as `MnfFile::read_entry`: never scan still-compressed Oodle data as if it were content.
        if self.mnf.header().version >= 3 && info.oodle_decompressed.is_none() && looks_oodle_compressed(info.file_bytes()) {
            c.oodle_errors.fetch_add(1, Relaxed);
            return;
        }

        let data = info.file_bytes();
        c.bytes_scanned.fetch_add(data.len() as u64, Relaxed);

        let mut found: Option<Found> = None;
        let mut via_records = false;
        if sh.opts.scan_records {
            let mut bad = 0u64;
            let n = for_each_record(data, &mut st.rec_buf, &mut bad, |rec, bytes| {
                sh.needles.scan_slice(bytes, Some(rec), &mut found);
                !sh.needles.complete(&found)
            });
            c.records.fetch_add(n as u64, Relaxed);
            c.record_errors.fetch_add(bad, Relaxed);
            via_records = n > 0;
        }
        if !via_records {
            sh.needles.scan_slice(data, None, &mut found);
        }

        if sh.needles.satisfied(&found) {
            let f = found.as_ref().expect("satisfied implies found");
            sh.report(Hit {
                container: self.path.to_path_buf(),
                entry: Some(EntryRef {
                    archive: e.archive_index,
                    index: e.index,
                    file_index: e.true_index(),
                    name: self.mnf.zosft_name(e).unwrap_or("").to_string(),
                    size: data.len(),
                }),
                matches: sh.needles.matches_of(f),
            });
        }
    }
}

fn fmt_bytes(b: u64) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: f64 = (1u64 << 20) as f64;
    let f = b as f64;
    if f >= GIB { format!("{:.2} GiB", f / GIB) } else { format!("{:.1} MiB", f / MIB) }
}

fn scan_mnf(sh: &Shared, path: &Path, tag: &str, threads: usize) -> Result<(), String> {
    let ps = path.to_string_lossy().into_owned();
    let mut mnf = MnfFile::new();
    mnf.load(&ps).map_err(|e| format!("could not load MNF: {e}"))?;
    let zosft_ok = mnf.load_zosft().is_ok();

    let table = mnf.file_table();
    let mut order: Vec<usize> = (0..table.len()).filter(|&i| table[i].compressed_size > 0).collect();
    order.sort_by_key(|&i| (table[i].archive_index, table[i].offset));
    let chunks = plan_chunks(table, &order);
    let total_bytes: u64 = order.iter().map(|&i| table[i].compressed_size as u64).sum();
    let total_entries = order.len();

    if !sh.opts.quiet {
        eprintln!(
            "{tag} {}: {} entries, {} compressed, {} chunks{}",
            path.display(),
            total_entries,
            fmt_bytes(total_bytes),
            chunks.len(),
            if zosft_ok { "" } else { " (no filenames: ZOSFT not loaded)" }
        );
    }

    let job = MnfJob { sh, mnf: &mnf, path, order, chunks };
    let base_bytes = sh.counters.bytes_read.load(Relaxed);
    let base_entries = sh.counters.entries.load(Relaxed);
    let t0 = Instant::now();

    let progress = || {
        let done = sh.counters.bytes_read.load(Relaxed) - base_bytes;
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        format!(
            "{tag} {:5.1}%  {} / {}  {:.0} MiB/s  {} of {} entries, {} hit(s), {} error(s)",
            done as f64 * 100.0 / total_bytes.max(1) as f64,
            fmt_bytes(done),
            fmt_bytes(total_bytes),
            done as f64 / (1u64 << 20) as f64 / secs,
            sh.counters.entries.load(Relaxed) - base_entries,
            total_entries,
            sh.counters.hits.load(Relaxed),
            sh.counters.read_errors.load(Relaxed) + sh.counters.oodle_errors.load(Relaxed),
        )
    };

    let job_ref = &job;
    let panics = run_parallel(threads, job.chunks.len(), if sh.opts.quiet { 0 } else { sh.opts.progress_secs }, &progress, || {
        let mut st = Worker::default();
        move |ci: usize| job_ref.process_chunk(ci, &mut st)
    });
    if panics > 0 {
        sh.note_error(format!("{}: {} worker thread(s) panicked; results for this MNF are incomplete", path.display(), panics));
    }
    if !sh.opts.quiet {
        eprintln!("{}", progress());
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Loose files
// ---------------------------------------------------------------------------------------------

fn ext_lower(p: &Path) -> Option<String> {
    p.extension().map(|e| e.to_string_lossy().to_lowercase())
}

/// `<dir>/gameNNNN.dat` belongs to `<dir>/game.mnf` when that MNF was found.
fn is_archive_dat(p: &Path, mnf_set: &BTreeSet<String>) -> bool {
    let (Some(stem), Some(dir)) = (p.file_stem().map(|s| s.to_string_lossy().into_owned()), p.parent()) else {
        return false;
    };
    if stem.len() <= 4 || !stem.is_char_boundary(stem.len() - 4) {
        return false;
    }
    let (prefix, digits) = stem.split_at(stem.len() - 4);
    digits.bytes().all(|b| b.is_ascii_digit()) && mnf_set.contains(&dir.join(format!("{prefix}.mnf")).to_string_lossy().to_lowercase())
}

fn discover(root: &Path, skip: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut mnfs = Vec::new();
    let mut others = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let p = ent.path();
            if skip.iter().any(|s| *s == p) {
                continue;
            }
            let Ok(ft) = ent.file_type() else { continue };
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                if ext_lower(&p).as_deref() == Some("mnf") { mnfs.push(p) } else { others.push(p) }
            }
        }
    }
    mnfs.sort();
    let mnf_set: BTreeSet<String> = mnfs.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
    let mut loose: Vec<PathBuf> = others.into_iter().filter(|p| !(ext_lower(p).as_deref() == Some("dat") && is_archive_dat(p, &mnf_set))).collect();
    loose.sort();
    (mnfs, loose)
}

fn scan_loose_file(sh: &Shared, path: &Path) {
    let mut found: Option<Found> = None;
    match File::open(path).and_then(|f| sh.needles.scan_reader(f, LOOSE_CHUNK, &mut found)) {
        Ok(bytes) => {
            sh.counters.loose_files.fetch_add(1, Relaxed);
            sh.counters.loose_bytes.fetch_add(bytes, Relaxed);
            if sh.needles.satisfied(&found) {
                sh.report(Hit { container: path.to_path_buf(), entry: None, matches: sh.needles.matches_of(found.as_ref().expect("satisfied implies found")) });
            }
        }
        Err(e) => {
            sh.counters.loose_errors.fetch_add(1, Relaxed);
            sh.note_error(format!("{}: {}", path.display(), e));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------------------------

fn try_load_oodle(opts: &ScanOptions, mnfs: &[PathBuf]) -> bool {
    let mut tries: Vec<Option<String>> = vec![opts.oodle_path.clone(), None, Some(opts.root.to_string_lossy().into_owned())];
    for m in mnfs {
        if let Some(parent) = m.parent() {
            let p = parent.to_string_lossy().into_owned();
            if !tries.contains(&Some(p.clone())) {
                tries.push(Some(p));
            }
        }
    }
    tries.iter().any(|t| load_oodle_lib_from(t.as_deref()))
}

/// Scans the whole folder. Hits are printed as they are found (unless `opts.quiet`) and returned
/// in the summary.
///
/// This turns off the console echo of `eso_common`'s logging for the rest of the process:
/// truncated/odd entries are expected in a full scan and are counted in the summary instead.
pub fn run(opts: &ScanOptions) -> Result<ScanSummary, String> {
    let started = Instant::now();
    if !opts.root.is_dir() {
        return Err(format!("'{}' is not a folder", opts.root.display()));
    }
    let needles = Needles::new(&opts.patterns, opts.case_sensitive, opts.utf16, opts.match_all, opts.context_bytes)?;
    eso_common::log::set_console_enabled(false);

    let threads = if opts.threads == 0 { thread::available_parallelism().map(|n| n.get()).unwrap_or(4) } else { opts.threads };

    let csv = match &opts.csv_path {
        Some(p) => {
            let mut w = BufWriter::new(File::create(p).map_err(|e| format!("cannot create '{}': {e}", p.display()))?);
            writeln!(w, "container,archive,entry_index,file_index,name,size,record,pattern,encoding,offset").map_err(|e| e.to_string())?;
            Some(w)
        }
        None => None,
    };

    let skip: Vec<PathBuf> = opts.csv_path.iter().cloned().collect();
    let (mut mnfs, loose) = discover(&opts.root, &skip);
    if let Some(f) = &opts.mnf_filter {
        let f = f.to_lowercase();
        mnfs.retain(|p| p.to_string_lossy().to_lowercase().contains(&f));
    }

    let oodle_loaded = try_load_oodle(opts, &mnfs);
    if !opts.quiet {
        eprintln!(
            "Searching for {:?} ({}), {} thread(s); {} MNF file(s), {} loose file(s).",
            needles.patterns(),
            if opts.match_all { "all must match" } else { "any may match" },
            threads,
            mnfs.len(),
            if opts.scan_loose { loose.len() } else { 0 }
        );
        if !oodle_loaded {
            eprintln!("WARNING: Oodle library not found - Oodle-compressed entries (version 3 archives) will be skipped. Use --oodle <dll or folder>.");
        }
    }

    let sh = Shared {
        opts,
        needles,
        counters: Counters::default(),
        sink: Mutex::new(SinkInner { hits: Vec::new(), csv }),
        errors: Mutex::new(Vec::new()),
        missing_dats: Mutex::new(BTreeSet::new()),
    };
    let mut mnf_load_errors = Vec::new();
    let mut mnf_files = 0u64;

    // Phase 1: loose files
    if opts.scan_loose && !loose.is_empty() {
        let progress = || format!("[loose] {} of {} files, {} hit(s)", sh.counters.loose_files.load(Relaxed), loose.len(), sh.counters.hits.load(Relaxed));
        let loose_ref = &loose;
        let sh_ref = &sh;
        let panics = run_parallel(threads, loose.len(), if opts.quiet { 0 } else { opts.progress_secs }, &progress, || move |i: usize| scan_loose_file(sh_ref, &loose_ref[i]));
        if panics > 0 {
            sh.note_error(format!("{panics} worker thread(s) panicked while scanning loose files"));
        }
    }

    // Phase 2: MNF archives, one at a time (each uses the whole pool)
    let total = mnfs.len();
    for (k, path) in mnfs.iter().enumerate() {
        let tag = format!("[{}/{}]", k + 1, total);
        match scan_mnf(&sh, path, &tag, threads) {
            Ok(()) => mnf_files += 1,
            Err(e) => {
                if !opts.quiet {
                    eprintln!("{tag} {}: {e}", path.display());
                }
                mnf_load_errors.push((path.clone(), e));
            }
        }
    }

    let c = &sh.counters;
    let summary = ScanSummary {
        oodle_loaded,
        mnf_files,
        mnf_load_errors,
        entries: c.entries.load(Relaxed),
        bytes_read: c.bytes_read.load(Relaxed),
        bytes_scanned: c.bytes_scanned.load(Relaxed),
        records: c.records.load(Relaxed),
        loose_files: c.loose_files.load(Relaxed),
        loose_bytes: c.loose_bytes.load(Relaxed),
        loose_errors: c.loose_errors.load(Relaxed),
        read_errors: c.read_errors.load(Relaxed),
        oodle_errors: c.oodle_errors.load(Relaxed),
        record_errors: c.record_errors.load(Relaxed),
        missing_dats: lock(&sh.missing_dats).iter().cloned().collect(),
        error_samples: lock(&sh.errors).clone(),
        hits: std::mem::take(&mut lock(&sh.sink).hits),
        elapsed: started.elapsed(),
    };
    Ok(summary)
}

/// Human-readable end-of-run report.
pub fn format_summary(s: &ScanSummary, opts: &ScanOptions) -> String {
    let mut o = String::new();
    o.push_str(&format!(
        "\nDone in {:.1} s: {} hit(s).\n  MNF: {} file(s), {} entries ({} compressed read, {} decompressed searched), {} EsoFileData records\n  Loose: {} file(s), {}\n",
        s.elapsed.as_secs_f64(),
        s.hits.len(),
        s.mnf_files,
        s.entries,
        fmt_bytes(s.bytes_read),
        fmt_bytes(s.bytes_scanned),
        s.records,
        s.loose_files,
        fmt_bytes(s.loose_bytes),
    ));
    if s.oodle_errors > 0 {
        o.push_str(&format!(
            "  WARNING: {} entries were Oodle-compressed but could not be decompressed{} - they were NOT searched.\n",
            s.oodle_errors,
            if s.oodle_loaded { "" } else { " (Oodle library not loaded; pass --oodle)" }
        ));
    }
    if s.read_errors > 0 {
        o.push_str(&format!("  {} entries could not be read/decompressed (a few are normally truncated in the game data).\n", s.read_errors));
    }
    if s.record_errors > 0 {
        o.push_str(&format!("  {} EsoFileData records failed to inflate.\n", s.record_errors));
    }
    if s.loose_errors > 0 {
        o.push_str(&format!("  {} loose files could not be read.\n", s.loose_errors));
    }
    for d in &s.missing_dats {
        o.push_str(&format!("  Missing DAT archive: {}\n", d.display()));
    }
    for (p, e) in &s.mnf_load_errors {
        o.push_str(&format!("  Could not scan {}: {}\n", p.display(), e));
    }
    if !s.error_samples.is_empty() {
        o.push_str("  First errors:\n");
        for e in s.error_samples.iter().take(8) {
            o.push_str(&format!("    {e}\n"));
        }
    }
    if s.hits.iter().any(|h| h.entry.is_some()) {
        o.push_str("\nTo extract an archive hit with EsoExtractData:\n  EsoExtractData <mnf> <outdir> -f <file index> --extractsubfile combined\n");
    }
    if s.hits.is_empty() {
        o.push_str(&format!(
            "\nNothing matched {:?}.{}\n",
            opts.patterns,
            if opts.match_all { " Try --any to see files containing only some of the strings." } else { "" }
        ));
    }
    o
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};

    fn needles(pats: &[&str], all: bool, utf16: bool) -> Needles {
        let p: Vec<String> = pats.iter().map(|s| s.to_string()).collect();
        Needles::new(&p, false, utf16, all, 8).unwrap()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = ZlibEncoder::new(Vec::new(), Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    #[test]
    fn any_vs_all_and_prefix_patterns() {
        let n_all = needles(&["Z_WINTERHOLDOVERLAND", "Z_WINTERHOLD"], true, false);
        let mut slot = None;
        n_all.scan_slice(b"xx z_winterholdoverland yy", None, &mut slot);
        // the shorter pattern is a prefix of the longer one: both must be reported
        assert!(n_all.satisfied(&slot));
        assert_eq!(n_all.matches_of(slot.as_ref().unwrap()).len(), 2);

        let n_all = needles(&["AAA", "BBB"], true, false);
        let mut slot = None;
        n_all.scan_slice(b"..AAA..", None, &mut slot);
        assert!(!n_all.satisfied(&slot));
        let n_any = needles(&["AAA", "BBB"], false, false);
        let mut slot = None;
        n_any.scan_slice(b"..AAA..", None, &mut slot);
        assert!(n_any.satisfied(&slot));
    }

    #[test]
    fn utf16_pattern_found_only_when_enabled() {
        let wide: Vec<u8> = "Z_ABC".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut slot = None;
        needles(&["Z_ABC"], true, false).scan_slice(&wide, None, &mut slot);
        assert!(slot.is_none());
        let n = needles(&["Z_ABC"], true, true);
        n.scan_slice(&wide, None, &mut slot);
        let m = n.matches_of(slot.as_ref().unwrap());
        assert_eq!(m[0].encoding, Encoding::Utf16Le);
    }

    #[test]
    fn reader_finds_matches_across_chunk_boundaries_once() {
        let n = needles(&["NEEDLE"], false, false);
        for chunk in [1usize, 2, 3, 5, 7, 64] {
            for pos in [0usize, 1, 5, 10, 20] {
                let mut data = vec![b'.'; 30];
                data[pos..pos + 6].copy_from_slice(b"NEEDLE");
                let mut slot = None;
                let total = n.scan_reader(&data[..], chunk, &mut slot).unwrap();
                assert_eq!(total, 30);
                let m = n.matches_of(slot.as_ref().unwrap());
                assert_eq!((m.len(), m[0].offset), (1, pos), "chunk {chunk} pos {pos}");
            }
        }
        let mut slot = None;
        n.scan_reader(&b"no match here"[..], 4, &mut slot).unwrap();
        assert!(slot.is_none());
    }

    fn eso_file_data(records: &[&[u8]]) -> Vec<u8> {
        let mut v = ESO_FILE_DATA_MAGIC.to_vec();
        v.extend(0x11u32.to_be_bytes());
        v.extend((records.len() as u32).to_be_bytes());
        v.extend(0u32.to_be_bytes());
        for r in records {
            let z = zlib(r);
            v.extend((r.len() as u32).to_be_bytes());
            v.extend((r.len() as u32).to_be_bytes());
            v.extend((z.len() as u32).to_be_bytes());
            v.extend(z);
        }
        v
    }

    #[test]
    fn records_are_inflated_and_numbered_from_one() {
        let data = eso_file_data(&[b"first", b"second Z_HIDDEN here", b"third"]);
        let n = needles(&["Z_HIDDEN"], true, false);

        // not visible in the raw container...
        let mut raw = None;
        n.scan_slice(&data, None, &mut raw);
        assert!(raw.is_none());

        // ...but found record by record
        let (mut buf, mut bad, mut slot) = (Vec::new(), 0u64, None);
        let count = for_each_record(&data, &mut buf, &mut bad, |rec, bytes| {
            n.scan_slice(bytes, Some(rec), &mut slot);
            true
        });
        assert_eq!((count, bad), (3, 0));
        let m = n.matches_of(slot.as_ref().unwrap());
        assert_eq!((m[0].record, m[0].offset), (Some(2), 7));
    }

    #[test]
    fn corrupt_records_are_counted_not_fatal() {
        let mut data = eso_file_data(&[b"hello Z_X", b"world"]);
        // damage the zlib stream of the first record
        data[16 + 12 + 2] ^= 0xFF;
        let (mut buf, mut bad) = (Vec::new(), 0u64);
        let mut seen = Vec::new();
        let count = for_each_record(&data, &mut buf, &mut bad, |rec, b| {
            seen.push((rec, b.to_vec()));
            true
        });
        assert_eq!(count, 2);
        assert_eq!(bad, 1);
        assert_eq!(seen, vec![(2, b"world".to_vec())]);
        // not an EsoFileData container at all
        assert_eq!(for_each_record(b"plain text", &mut buf, &mut bad, |_, _| true), 0);
    }

    #[test]
    fn chunks_stay_in_one_archive_and_respect_limits() {
        let mk = |archive, offset, cs| MnfFileEntry { archive_index: archive, offset, compressed_size: cs, ..Default::default() };
        let mut table = vec![mk(0, 0, 10), mk(0, 10, 10), mk(1, 0, 10), mk(1, 10, 10), mk(1, 20, 10)];
        let order: Vec<usize> = (0..table.len()).collect();
        let c = plan_chunks(&table, &order);
        assert_eq!(c, vec![0..2, 2..5]);

        // byte limit: the entry that fills a chunk closes it, the next entry starts a new one
        table.push(mk(1, 30, CHUNK_MAX_BYTES as u32));
        table.push(mk(1, 40, 10));
        let order: Vec<usize> = (0..table.len()).collect();
        let c = plan_chunks(&table, &order);
        assert_eq!(c, vec![0..2, 2..6, 6..7]);
        let table2: Vec<MnfFileEntry> = (0..CHUNK_MAX_ENTRIES as u32 + 1).map(|i| mk(3, i, 1)).collect();
        let order2: Vec<usize> = (0..table2.len()).collect();
        assert_eq!(plan_chunks(&table2, &order2), vec![0..CHUNK_MAX_ENTRIES, CHUNK_MAX_ENTRIES..CHUNK_MAX_ENTRIES + 1]);
        assert!(plan_chunks(&[], &[]).is_empty());
    }

    // ---- end-to-end: a real (synthetic) MNF + DAT on disk --------------------------------------

    /// Builds a version-2 MNF holding the given `(size, compressed size, offset, compress type)`
    /// entries, all in archive 1. Layout per `MnfFile::read_header` / `MnfBlock::read`.
    fn build_mnf(entries: &[(u32, u32, u32, u8)]) -> Vec<u8> {
        let n = entries.len();
        let (mut d1, mut d2, mut d3) = (Vec::new(), Vec::new(), Vec::new());
        for (i, &(size, csize, offset, ctype)) in entries.iter().enumerate() {
            d1.extend((0x8000_0000u32 | i as u32).to_le_bytes());
            d2.extend((i as u32 + 1).to_le_bytes());
            d2.extend(0u32.to_le_bytes());
            for v in [size, csize, 0x1000 + i as u32, offset] {
                d3.extend(v.to_le_bytes());
            }
            d3.extend([ctype, 1, 0, 0]);
        }
        let mut out = b"MES2".to_vec();
        out.extend(2u16.to_le_bytes()); // version
        out.push(1); // file count
        out.extend(1u32.to_le_bytes()); // type
        out.extend(0u32.to_le_bytes()); // data size
        out.extend(3u16.to_be_bytes()); // block id
        for v in [0u32, 0, 0, n as u32] {
            out.extend(v.to_be_bytes());
        }
        for d in [d1, d2, d3] {
            let z = zlib(&d);
            out.extend((d.len() as u32).to_be_bytes());
            out.extend((z.len() as u32).to_be_bytes());
            out.extend(z);
        }
        out
    }

    #[test]
    fn end_to_end_synthetic_install() {
        let dir = std::env::temp_dir().join(format!("eso_scan_e2e_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("game/client")).unwrap();
        fs::create_dir_all(dir.join("extra")).unwrap();

        let both: &[u8] = b"junk\0Z_THEWAILINGPRISON02\0Fp\0\x14\0Z_WINTERHOLDOVERLAND\0junk";
        let only_one: &[u8] = b"decoy Z_WINTERHOLDOVERLAND only";
        let record_container = eso_file_data(&[b"nothing", b"here is Z_THEWAILINGPRISON02", b"and Z_WINTERHOLDOVERLAND later"]);
        let zbody = zlib(&both.to_ascii_lowercase());

        // (blob bytes as stored in the DAT, uncompressed size, compress type)
        let blobs: Vec<(Vec<u8>, u32, u8)> = vec![
            (both.to_vec(), both.len() as u32, 0),                       // 0: stored, both strings
            (zbody.clone(), both.len() as u32, 1),                       // 1: zlib, lower-cased
            (record_container.clone(), record_container.len() as u32, 0), // 2: EsoFileData, strings in records 2 and 3
            (only_one.to_vec(), only_one.len() as u32, 0),               // 3: decoy
        ];
        let mut dat = vec![0u8; 16];
        let mut entries = Vec::new();
        for (bytes, size, ctype) in &blobs {
            entries.push((*size, bytes.len() as u32, dat.len() as u32, *ctype));
            dat.extend_from_slice(bytes);
        }
        fs::write(dir.join("game/client/game.mnf"), build_mnf(&entries)).unwrap();
        fs::write(dir.join("game/client/game0001.dat"), &dat).unwrap();
        fs::write(dir.join("extra/readme.txt"), b"loose Z_THEWAILINGPRISON02 and Z_WINTERHOLDOVERLAND text").unwrap();
        // an archive DAT must not be treated as a loose file (its stored blob would hit again)
        // and an unrelated .dat must be searched
        fs::write(dir.join("extra/other.dat"), b"Z_THEWAILINGPRISON02 Z_WINTERHOLDOVERLAND").unwrap();

        let mut opts = ScanOptions::new(&dir);
        opts.quiet = true;
        opts.threads = 3;
        opts.progress_secs = 0;
        let s = run(&opts).unwrap();

        assert_eq!(s.mnf_files, 1, "{:?}", s.mnf_load_errors);
        assert_eq!(s.entries, 4);
        assert_eq!(s.records, 3);
        assert_eq!((s.read_errors, s.oodle_errors, s.record_errors), (0, 0, 0), "{:?}", s.error_samples);

        let mut arch: Vec<u32> = s.hits.iter().filter_map(|h| h.entry.as_ref().map(|e| e.index)).collect();
        arch.sort();
        assert_eq!(arch, vec![0, 1, 2], "decoy (entry 3) must not match in all-mode");

        let rec_hit = s.hits.iter().find(|h| h.entry.as_ref().is_some_and(|e| e.index == 2)).unwrap();
        let recs: Vec<Option<usize>> = rec_hit.matches.iter().filter(|m| m.pattern != "Z_WINTERHOLD").map(|m| m.record).collect();
        assert_eq!(recs, vec![Some(2), Some(3)]);

        let mut loose: Vec<String> = s.hits.iter().filter(|h| h.entry.is_none()).map(|h| h.container.file_name().unwrap().to_string_lossy().into_owned()).collect();
        loose.sort();
        assert_eq!(loose, vec!["other.dat", "readme.txt"]);

        // any-mode also reports the decoy
        opts.match_all = false;
        opts.scan_loose = false;
        let s = run(&opts).unwrap();
        let mut idx: Vec<u32> = s.hits.iter().filter_map(|h| h.entry.as_ref().map(|e| e.index)).collect();
        idx.sort();
        assert_eq!(idx, vec![0, 1, 2, 3]);

        // CSV output and records switched off (container is then searched raw and finds nothing)
        opts.match_all = true;
        opts.scan_records = false;
        opts.csv_path = Some(dir.join("hits.csv"));
        let s = run(&opts).unwrap();
        let mut idx: Vec<u32> = s.hits.iter().filter_map(|h| h.entry.as_ref().map(|e| e.index)).collect();
        idx.sort();
        assert_eq!(idx, vec![0, 1], "without record scanning the EsoFileData entry is invisible");
        let csv = fs::read_to_string(dir.join("hits.csv")).unwrap();
        assert!(csv.lines().count() > 1 && csv.contains("Z_THEWAILINGPRISON02"));

        let _ = fs::remove_dir_all(&dir);
    }
}

//! Parallel, streaming, line-oriented search.
//!
//! The file is split into fixed-size chunks. Worker threads claim chunks in
//! order, read them with positional reads, and scan them with a compiled
//! regex. A chunk "owns" every line that *starts* inside it; a line that runs
//! past the end of the chunk is finished by reading a little further. Each
//! chunk also counts its newlines, so a collector thread can turn per-chunk
//! relative line numbers into absolute ones (and build a sparse line index as
//! a by-product).
//!
//! Reads are *issued* in chunk order through an [`IoGate`]. On a hard drive
//! only one read is allowed in flight, which keeps the head streaming; on an
//! SSD several reads overlap to keep the device queue full.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use memchr::{memchr, memrchr};
use regex::bytes::{Regex, RegexBuilder};

use crate::fileio::{open_read, read_full_at};
use crate::lines::LineIndex;

/// Bytes per work unit. Large enough for efficient I/O, small enough that
/// `threads * chunk` stays modest in memory.
pub const DEFAULT_CHUNK_SIZE: usize = 8 << 20;
/// How many hits are kept for display / export (24 bytes each).
pub const DEFAULT_MAX_HITS: usize = 5_000_000;
/// A line that keeps going this far past its chunk is cut off for searching.
const MAX_LINE_SPILL: usize = 256 << 20;

/// What to search for and how.
#[derive(Clone, Debug)]
pub struct SearchOptions {
    /// One or more search texts. Empty ones are ignored.
    pub patterns: Vec<String>,
    /// With several search texts: a line must contain every one of them.
    /// Otherwise a line matches if it contains any of them.
    pub match_all: bool,
    /// Treat the search texts as regular expressions instead of plain text.
    pub regex: bool,
    pub case_sensitive: bool,
    /// Only match whole words (ASCII word boundaries).
    pub whole_word: bool,
    /// Keep at most this many hits; matching lines beyond it are still counted.
    pub max_hits: usize,
    /// Worker threads; 0 = pick automatically.
    pub threads: usize,
    /// Chunk reads allowed in flight at once (1 = strictly sequential).
    pub io_parallelism: usize,
    pub chunk_size: usize,
}

impl SearchOptions {
    /// Search for one text.
    pub fn new(pattern: impl Into<String>) -> Self {
        Self::any_of([pattern])
    }

    /// Search for lines containing any of `patterns`.
    pub fn any_of<S: Into<String>>(patterns: impl IntoIterator<Item = S>) -> Self {
        SearchOptions {
            patterns: patterns.into_iter().map(Into::into).collect(),
            match_all: false,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            max_hits: DEFAULT_MAX_HITS,
            threads: 0,
            io_parallelism: 1,
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Search for lines containing all of `patterns`.
    pub fn all_of<S: Into<String>>(patterns: impl IntoIterator<Item = S>) -> Self {
        SearchOptions { match_all: true, ..Self::any_of(patterns) }
    }

    /// The search texts that are actually used (the non-empty ones).
    pub fn terms(&self) -> Vec<&str> {
        self.patterns.iter().map(String::as_str).filter(|p| !p.is_empty()).collect()
    }
}

/// One matching line. Only positions are stored; text is read on demand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Byte offset where the matching line starts.
    pub line_start: u64,
    /// 1-based line number.
    pub line_no: u64,
    /// Offset of the first match, relative to `line_start`.
    pub match_start: u32,
    /// Length of the first match in bytes (clipped to the line).
    pub match_len: u32,
}

impl Hit {
    pub fn match_offset(&self) -> u64 {
        self.line_start + self.match_start as u64
    }
}

/// Above this many search texts, matches are highlighted in a single color.
const MAX_COLORED_TERMS: usize = 32;

/// The regex source for one search text, with the options applied.
fn term_source(pattern: &str, opts: &SearchOptions) -> String {
    let pat = if opts.regex { pattern.to_string() } else { regex::escape(pattern) };
    if opts.whole_word {
        // ASCII boundaries keep the fast DFA engines usable on any input.
        format!(r"(?-u:\b)(?:{pat})(?-u:\b)")
    } else {
        pat
    }
}

fn compile(source: &str, opts: &SearchOptions) -> Result<Regex, String> {
    RegexBuilder::new(source)
        .case_insensitive(!opts.case_sensitive)
        .multi_line(true)
        .crlf(true)
        .size_limit(256 << 20)
        .dfa_size_limit(256 << 20)
        .build()
        .map_err(|e| match e {
            regex::Error::Syntax(s) => format!("Invalid regular expression:\n{s}"),
            regex::Error::CompiledTooBig(_) => "That search is too big to run.".into(),
            other => other.to_string(),
        })
}

/// Compiles a single search text with the flags in `opts`. Errors are
/// human-readable.
pub fn build_regex(pattern: &str, opts: &SearchOptions) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("Type something to search for.".into());
    }
    compile(&term_source(pattern, opts), opts)
}

/// The compiled form of a search: one or more search texts, matched per line
/// as "any of them" or "all of them".
#[derive(Clone, Debug)]
pub struct Matcher {
    /// Matches any one of the search texts.
    any: Regex,
    /// One regex per search text.
    each: Vec<Regex>,
    /// Every search text must be on the line (and there is more than one).
    match_all: bool,
    /// Like `any`, but each search text sits in its own capture group, so a
    /// match tells which text it came from. `None` with one text or very many.
    tagged: Option<(Regex, Vec<usize>)>,
}

impl Matcher {
    pub fn new(opts: &SearchOptions) -> Result<Matcher, String> {
        let terms = opts.terms();
        if terms.is_empty() {
            return Err("Type something to search for.".into());
        }
        let each = terms
            .iter()
            .enumerate()
            .map(|(i, t)| {
                build_regex(t, opts).map_err(|e| {
                    if terms.len() == 1 {
                        e
                    } else {
                        format!("Search text {} ({t:?}): {e}", i + 1)
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sources: Vec<String> = terms.iter().map(|t| term_source(t, opts)).collect();
        let any = if each.len() == 1 {
            each[0].clone()
        } else {
            let alternation: Vec<String> = sources.iter().map(|s| format!("(?:{s})")).collect();
            compile(&alternation.join("|"), opts)?
        };
        let tagged = if (2..=MAX_COLORED_TERMS).contains(&each.len()) {
            // Each text adds its wrapper group plus the groups it contains.
            let mut groups = Vec::with_capacity(each.len());
            let mut next = 1;
            for re in &each {
                groups.push(next);
                next += re.captures_len();
            }
            let alternation: Vec<String> = sources.iter().map(|s| format!("({s})")).collect();
            Some((compile(&alternation.join("|"), opts)?, groups))
        } else {
            None
        };
        Ok(Matcher { any, each, match_all: opts.match_all && terms.len() > 1, tagged })
    }

    /// Number of search texts.
    pub fn terms(&self) -> usize {
        self.each.len()
    }

    /// For "all of them" searches: checks one `line` (with its terminator, if
    /// any; `content_len` excludes it). When every search text matches the
    /// line, returns the leftmost match, relative to the line.
    fn check_line(&self, line: &[u8], content_len: usize) -> Option<Range<usize>> {
        let mut best: Option<Range<usize>> = None;
        for re in &self.each {
            let m = re.find(line).filter(|m| m.start() <= content_len)?;
            if best.as_ref().is_none_or(|b| m.start() < b.start) {
                best = Some(m.range());
            }
        }
        best
    }

    /// Up to `limit` non-empty matches in `bytes`, each with the index of the
    /// search text it belongs to (always 0 when there are too many texts to
    /// tell apart).
    pub fn highlights(&self, bytes: &[u8], limit: usize) -> Vec<(Range<usize>, usize)> {
        match &self.tagged {
            Some((re, groups)) => re
                .captures_iter(bytes)
                .filter_map(|c| {
                    let m = c.get(0).filter(|m| !m.is_empty())?;
                    let term = groups.iter().position(|&g| c.get(g).is_some()).unwrap_or(0);
                    Some((m.range(), term))
                })
                .take(limit)
                .collect(),
            None => self.any.find_iter(bytes).filter(|m| !m.is_empty()).take(limit).map(|m| (m.range(), 0)).collect(),
        }
    }
}

/// Live counters, readable from any thread while a search runs.
pub struct Progress {
    pub total_bytes: u64,
    /// Bytes fully processed, counted contiguously from the start of the file.
    pub bytes_done: AtomicU64,
    pub matched_lines: AtomicU64,
    pub started: Instant,
}

/// Final outcome of a search.
#[derive(Clone, Debug)]
pub struct SearchSummary {
    pub matched_lines: u64,
    pub stored_hits: u64,
    pub bytes_searched: u64,
    /// Total line count, known only if the whole file was searched.
    pub total_lines: Option<u64>,
    pub elapsed: Duration,
    pub cancelled: bool,
    pub error: Option<String>,
    /// Newline counts per chunk for the searched prefix of the file.
    pub line_index: LineIndex,
    /// Some line was longer than 256 MB and was only partly searched.
    pub truncated_long_lines: bool,
}

pub enum SearchEvent {
    /// More hits, in file order.
    Hits(Vec<Hit>),
    Finished(SearchSummary),
}

/// A running search. Dropping it cancels the search.
pub struct SearchHandle {
    pub progress: Arc<Progress>,
    pub events: Receiver<SearchEvent>,
    pub path: PathBuf,
    shared: Arc<Shared>,
}

impl SearchHandle {
    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

impl Drop for SearchHandle {
    fn drop(&mut self) {
        self.shared.cancel();
    }
}

/// Default worker count: all cores, capped so memory stays bounded.
pub fn default_threads() -> usize {
    thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 16)
}

/// Starts searching `path` in the background. Results arrive on
/// [`SearchHandle::events`]; `wake` is called after each event so a UI can
/// repaint.
pub fn start_search(
    path: &Path,
    opts: SearchOptions,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) -> Result<SearchHandle, String> {
    let matcher = Matcher::new(&opts)?;
    let file = open_read(path, true).map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    let file_len = file
        .metadata()
        .map_err(|e| format!("Cannot read size of {}: {e}", path.display()))?
        .len();
    let chunk_size = opts.chunk_size.max(1);
    let num_chunks = file_len.div_ceil(chunk_size as u64) as usize;
    let threads = if opts.threads == 0 { default_threads() } else { opts.threads };
    let threads = threads.clamp(1, num_chunks.max(1));

    let shared = Arc::new(Shared {
        cancel: AtomicBool::new(false),
        hits_full: AtomicBool::new(false),
        next_chunk: AtomicUsize::new(0),
        gate: IoGate::new(opts.io_parallelism.max(1)),
        matcher,
        chunk_size,
        num_chunks,
        file_len,
        max_hits: opts.max_hits,
    });
    let progress = Arc::new(Progress {
        total_bytes: file_len,
        bytes_done: AtomicU64::new(0),
        matched_lines: AtomicU64::new(0),
        started: Instant::now(),
    });

    let (res_tx, res_rx) = crossbeam_channel::unbounded::<ChunkResult>();
    let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<SearchEvent>();

    let mut first_file = Some(file);
    for w in 0..threads {
        // Every worker gets its own handle so reads can truly overlap.
        let handle = match first_file.take() {
            Some(f) => Ok(f),
            None => open_read(path, true),
        };
        let shared = Arc::clone(&shared);
        let tx = res_tx.clone();
        thread::Builder::new()
            .name(format!("search-{w}"))
            .spawn(move || match handle {
                Ok(f) => worker(&shared, &f, &tx),
                Err(e) => {
                    let _ = tx.send(ChunkResult::failed(format!("Cannot open file: {e}")));
                }
            })
            .map_err(|e| format!("Cannot start search thread: {e}"))?;
    }
    drop(res_tx);

    {
        let shared = Arc::clone(&shared);
        let progress = Arc::clone(&progress);
        let path = path.to_path_buf();
        thread::Builder::new()
            .name("search-collect".into())
            .spawn(move || collector(&shared, &progress, &path, res_rx, ev_tx, wake))
            .map_err(|e| format!("Cannot start search thread: {e}"))?;
    }

    Ok(SearchHandle { progress, events: ev_rx, path: path.to_path_buf(), shared })
}

struct Shared {
    cancel: AtomicBool,
    hits_full: AtomicBool,
    next_chunk: AtomicUsize,
    gate: IoGate,
    matcher: Matcher,
    chunk_size: usize,
    num_chunks: usize,
    file_len: u64,
    max_hits: usize,
}

impl Shared {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.gate.wake_all();
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

/// Lets chunk reads start strictly in chunk order, with at most `max` of them
/// in flight at a time.
struct IoGate {
    state: Mutex<(usize, usize)>, // (next ticket allowed to start, reads in flight)
    cv: Condvar,
    max: usize,
}

struct IoPermit<'a>(&'a IoGate);

impl Drop for IoPermit<'_> {
    fn drop(&mut self) {
        let mut st = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        st.1 -= 1;
        drop(st);
        self.0.cv.notify_all();
    }
}

impl IoGate {
    fn new(max: usize) -> Self {
        IoGate { state: Mutex::new((0, 0)), cv: Condvar::new(), max }
    }

    fn acquire<'a>(&'a self, ticket: usize, cancel: &AtomicBool) -> Option<IoPermit<'a>> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            if st.0 == ticket && st.1 < self.max {
                st.0 += 1;
                st.1 += 1;
                drop(st);
                self.cv.notify_all();
                return Some(IoPermit(self));
            }
            st = self
                .cv
                .wait_timeout(st, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn wake_all(&self) {
        self.cv.notify_all();
    }
}

struct ChunkResult {
    index: usize,
    chunk_len: u64,
    newlines: u64,
    matched: u64,
    /// `line_no` is relative: newlines between chunk start and the line.
    hits: Vec<Hit>,
    truncated: bool,
    error: Option<String>,
}

impl ChunkResult {
    fn failed(msg: String) -> Self {
        ChunkResult {
            index: usize::MAX,
            chunk_len: 0,
            newlines: 0,
            matched: 0,
            hits: Vec::new(),
            truncated: false,
            error: Some(msg),
        }
    }
}

enum ChunkError {
    Cancelled,
    Io(io::Error),
}

impl From<io::Error> for ChunkError {
    fn from(e: io::Error) -> Self {
        ChunkError::Io(e)
    }
}

fn worker(shared: &Shared, file: &File, tx: &Sender<ChunkResult>) {
    let mut buf = Vec::new();
    loop {
        if shared.cancelled() {
            return;
        }
        let index = shared.next_chunk.fetch_add(1, Ordering::Relaxed);
        if index >= shared.num_chunks {
            return;
        }
        let res = match process_chunk(shared, file, &mut buf, index) {
            Ok(r) => r,
            Err(ChunkError::Cancelled) => return,
            Err(ChunkError::Io(e)) => {
                let offset = index as u64 * shared.chunk_size as u64;
                ChunkResult::failed(format!("Read error near byte {offset}: {e}"))
            }
        };
        if tx.send(res).is_err() {
            return;
        }
    }
}

fn process_chunk(
    shared: &Shared,
    file: &File,
    buf: &mut Vec<u8>,
    index: usize,
) -> Result<ChunkResult, ChunkError> {
    let csize = shared.chunk_size as u64;
    let start = index as u64 * csize;
    let end = (start + csize).min(shared.file_len);
    // Read one byte early so we can tell whether `start` begins a line.
    let read_start = if index == 0 { 0 } else { start - 1 };
    let main_len = (end - read_start) as usize;
    if buf.len() < main_len {
        buf.resize(main_len, 0);
    }
    let n = {
        let _permit = shared.gate.acquire(index, &shared.cancel).ok_or(ChunkError::Cancelled)?;
        read_full_at(file, &mut buf[..main_len], read_start)?
    };

    let mut res = ChunkResult {
        index,
        chunk_len: end - start,
        newlines: 0,
        matched: 0,
        hits: Vec::new(),
        truncated: false,
        error: None,
    };
    let s_rel = (start - read_start) as usize;
    if n <= s_rel {
        return Ok(res); // file shrank underneath us
    }
    res.newlines = bytecount::count(&buf[s_rel..n], b'\n') as u64;

    // First line that starts inside this chunk.
    let first = if index == 0 { 0 } else { memchr(b'\n', &buf[..n]).map_or(n, |p| p + 1) };
    if first >= n {
        return Ok(res); // no line starts here (one giant line passes through)
    }

    // Finish the last line if it runs past the chunk end.
    let mut region_end = n;
    if buf[n - 1] != b'\n' && read_start + (n as u64) < shared.file_len {
        let mut pos = read_start + n as u64;
        let mut step = 64usize << 10;
        loop {
            if region_end - n >= MAX_LINE_SPILL {
                res.truncated = true;
                break;
            }
            if shared.cancelled() {
                return Err(ChunkError::Cancelled);
            }
            if buf.len() < region_end + step {
                buf.resize(region_end + step, 0);
            }
            let got = read_full_at(file, &mut buf[region_end..region_end + step], pos)?;
            if got == 0 {
                break;
            }
            if let Some(p) = memchr(b'\n', &buf[region_end..region_end + got]) {
                region_end += p + 1;
                break;
            }
            region_end += got;
            pos += got as u64;
            step = (step * 2).min(8 << 20);
        }
    }

    let region = &buf[first..region_end];
    let base = read_start + first as u64;
    let mut line_rel = bytecount::count(&buf[s_rel..first], b'\n') as u64;
    let mut counted_to = 0usize;
    let store = !shared.hits_full.load(Ordering::Relaxed);
    let mut pos = 0usize;
    let matcher = &shared.matcher;
    while pos <= region.len() {
        let Some(m) = matcher.any.find_at(region, pos) else { break };
        let (mut ms, mut me) = (m.start(), m.end());
        let ls = memrchr(b'\n', &region[..ms]).map_or(0, |p| p + 1);
        if ls >= region.len() {
            break; // empty match at the very start of the next chunk's line
        }
        let le = memchr(b'\n', &region[ms..]).map_or(region.len(), |p| ms + p);
        if matcher.match_all {
            // The line has one of the texts; it needs every one of them.
            let line = &region[ls..(le + 1).min(region.len())];
            match matcher.check_line(line, le - ls) {
                Some(r) => (ms, me) = (ls + r.start, ls + r.end),
                None => {
                    pos = le + 1;
                    continue;
                }
            }
        }
        line_rel += bytecount::count(&region[counted_to..ls], b'\n') as u64;
        counted_to = ls;
        res.matched += 1;
        if store && res.hits.len() < shared.max_hits {
            res.hits.push(Hit {
                line_start: base + ls as u64,
                line_no: line_rel,
                match_start: (ms - ls).min(u32::MAX as usize) as u32,
                match_len: (me.min(le).saturating_sub(ms)).min(u32::MAX as usize) as u32,
            });
        }
        pos = le + 1;
    }
    Ok(res)
}

fn collector(
    shared: &Shared,
    progress: &Progress,
    path: &Path,
    rx: Receiver<ChunkResult>,
    ev_tx: Sender<SearchEvent>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let notify = || {
        if let Some(w) = &wake {
            w();
        }
    };
    let mut pending: BTreeMap<usize, ChunkResult> = BTreeMap::new();
    let mut next = 0usize;
    let mut newlines_before: Vec<u64> = Vec::with_capacity(shared.num_chunks + 1);
    newlines_before.push(0);
    let (mut stored, mut matched, mut done_bytes) = (0usize, 0u64, 0u64);
    let mut error: Option<String> = None;
    let mut truncated = false;

    for mut r in rx.iter() {
        if let Some(e) = r.error.take() {
            error.get_or_insert(e);
            shared.cancel();
            continue;
        }
        pending.insert(r.index, r);
        while let Some(r) = pending.remove(&next) {
            let base = *newlines_before.last().unwrap();
            matched += r.matched;
            truncated |= r.truncated;
            if !r.hits.is_empty() && stored < shared.max_hits {
                let mut hits = r.hits;
                hits.truncate(shared.max_hits - stored);
                for h in &mut hits {
                    h.line_no += base + 1;
                }
                stored += hits.len();
                if stored >= shared.max_hits {
                    shared.hits_full.store(true, Ordering::Relaxed);
                }
                let _ = ev_tx.send(SearchEvent::Hits(hits));
            }
            newlines_before.push(base + r.newlines);
            done_bytes += r.chunk_len;
            progress.bytes_done.store(done_bytes, Ordering::Relaxed);
            progress.matched_lines.store(matched, Ordering::Relaxed);
            next += 1;
        }
    }

    let completed = error.is_none() && next == shared.num_chunks;
    let total_lines = if completed {
        let newlines = *newlines_before.last().unwrap();
        let unterminated_last = shared.file_len > 0 && last_byte(path, shared.file_len) != Some(b'\n');
        Some(newlines + unterminated_last as u64)
    } else {
        None
    };
    let summary = SearchSummary {
        matched_lines: matched,
        stored_hits: stored as u64,
        bytes_searched: done_bytes,
        total_lines,
        elapsed: progress.started.elapsed(),
        cancelled: !completed && error.is_none(),
        error,
        line_index: LineIndex {
            chunk_size: shared.chunk_size as u64,
            file_len: shared.file_len,
            newlines_before,
        },
        truncated_long_lines: truncated,
    };
    let _ = ev_tx.send(SearchEvent::Finished(summary));
    notify();
}

fn last_byte(path: &Path, len: u64) -> Option<u8> {
    let f = open_read(path, false).ok()?;
    let mut b = [0u8; 1];
    (read_full_at(&f, &mut b, len - 1).ok()? == 1).then_some(b[0])
}

/// Runs a search to completion on the calling thread. Handy for tests and
/// command-line use.
pub fn search_blocking(path: &Path, opts: SearchOptions) -> Result<(Vec<Hit>, SearchSummary), String> {
    let handle = start_search(path, opts, None)?;
    let mut hits = Vec::new();
    for ev in handle.events.iter() {
        match ev {
            SearchEvent::Hits(h) => hits.extend(h),
            SearchEvent::Finished(s) => return Ok((hits, s)),
        }
    }
    Err("search ended unexpectedly".into())
}

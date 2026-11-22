//! Checks the chunked parallel search against a simple line-by-line reference.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use hfr::search::{build_regex, search_blocking, Hit, Matcher, SearchOptions};

struct TempFile(PathBuf);

impl TempFile {
    fn new(content: &[u8]) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "hfr-test-{}-{}.txt",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::File::create(&p).unwrap().write_all(content).unwrap();
        TempFile(p)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Splits like a text editor: a trailing "\n" does not start another line.
/// Each search text is checked on its own: a line matches if any (or, with
/// `match_all`, every) text is found, and the hit is the leftmost match, the
/// earlier text winning a tie.
fn reference(content: &[u8], opts: &SearchOptions) -> (Vec<Hit>, u64) {
    let res: Vec<_> = opts.terms().iter().map(|t| build_regex(t, opts).unwrap()).collect();
    let mut hits = Vec::new();
    let mut offset = 0usize;
    let mut line_no = 0u64;
    while offset < content.len() {
        line_no += 1;
        let end = content[offset..].iter().position(|&b| b == b'\n').map_or(content.len(), |p| offset + p);
        // Include the "\n" so CRLF-aware anchors see the real terminator.
        let line = &content[offset..(end + 1).min(content.len())];
        let content_len = end - offset;
        let found: Vec<_> = res.iter().map(|re| re.find(line).filter(|m| m.start() <= content_len)).collect();
        let ok = if opts.match_all { found.iter().all(Option::is_some) } else { found.iter().any(Option::is_some) };
        if let Some(m) = found.iter().flatten().min_by_key(|m| m.start()).filter(|_| ok) {
            hits.push(Hit {
                line_start: offset as u64,
                line_no,
                match_start: m.start() as u32,
                match_len: (m.end().min(content_len) - m.start()) as u32,
            });
        }
        offset = end + 1;
    }
    (hits, line_no)
}

fn check(content: &[u8], opts: SearchOptions) {
    let file = TempFile::new(content);
    let (want, want_lines) = reference(content, &opts);
    for chunk_size in [1usize, 2, 3, 7, 16, 64, 1000, 1 << 20] {
        for (threads, io) in [(1, 1), (3, 1), (4, 2), (8, 8)] {
            let o = SearchOptions { chunk_size, threads, io_parallelism: io, ..opts.clone() };
            let (got, summary) = search_blocking(&file.0, o).unwrap();
            let ctx = format!("pattern {:?} chunk {chunk_size} threads {threads} io {io}", opts.patterns);
            assert_eq!(got, want, "{ctx}");
            assert_eq!(summary.matched_lines, want.len() as u64, "{ctx}");
            assert_eq!(summary.total_lines, Some(want_lines), "{ctx}");
            assert!(summary.line_index.is_complete(), "{ctx}");
            assert!(summary.error.is_none() && !summary.cancelled, "{ctx}");
        }
    }
}

fn opts(pattern: &str) -> SearchOptions {
    SearchOptions::new(pattern)
}

const SAMPLE: &str = "alpha beta\nGamma delta\n\nepsilon ALPHA\r\nzeta alphabet\n  indented alpha\nlast line no newline alpha";

#[test]
fn literal_case_insensitive() {
    check(SAMPLE.as_bytes(), opts("alpha"));
}

#[test]
fn literal_case_sensitive() {
    check(SAMPLE.as_bytes(), SearchOptions { case_sensitive: true, ..opts("alpha") });
}

#[test]
fn whole_word() {
    check(SAMPLE.as_bytes(), SearchOptions { whole_word: true, ..opts("alpha") });
}

#[test]
fn regex_anchors_and_crlf() {
    for p in [r"^\w+", r"alpha$", r"^$", r"^", r"a.p", r"\d+", r"l{2,}", r"(?i)ZETA|gamma", r"x*"] {
        check(SAMPLE.as_bytes(), SearchOptions { regex: true, ..opts(p) });
    }
}

#[test]
fn trailing_newline_and_empty_file() {
    check(b"a\nb\na\n", opts("a"));
    check(b"\n\n\n", SearchOptions { regex: true, ..opts("^$") });
    check(b"", opts("a"));
    check(b"a", opts("a"));
}

#[test]
fn long_lines_cross_many_chunks() {
    let mut s = String::new();
    for i in 0..50 {
        s.push_str(&"x".repeat(i * 37 % 300));
        if i % 7 == 0 {
            s.push_str("needle");
        }
        s.push_str(&"y".repeat(i * 11 % 90));
        s.push('\n');
    }
    check(s.as_bytes(), opts("needle"));
    check(s.as_bytes(), SearchOptions { regex: true, ..opts(r"y+$") });
}

#[test]
fn unicode_text() {
    let s = "héllo wörld\nПривет мир\n日本語のテキスト\nmixed ПРИВЕТ\n";
    check(s.as_bytes(), opts("привет"));
    check(s.as_bytes(), opts("テキスト"));
}

#[test]
fn many_lines_random() {
    // Deterministic pseudo-random text with lots of lines.
    let mut x: u64 = 0x2545F4914F6CDD1D;
    let mut s = Vec::new();
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let len = (x % 60) as usize;
        for k in 0..len {
            s.push(b"abcdefghij  ERROR\r"[(x as usize >> (k % 40)) % 18]);
        }
        s.push(b'\n');
    }
    check(&s, opts("error"));
    check(&s, SearchOptions { regex: true, ..opts(r"^a.*j$") });
}

#[test]
fn max_hits_caps_storage_not_count() {
    let content: Vec<u8> = (0..1000).flat_map(|i| format!("row {i}\n").into_bytes()).collect();
    let file = TempFile::new(&content);
    let o = SearchOptions { max_hits: 10, chunk_size: 64, threads: 4, ..opts("row") };
    let (hits, summary) = search_blocking(&file.0, o).unwrap();
    assert_eq!(hits.len(), 10);
    assert_eq!(hits[9].line_no, 10);
    assert_eq!(summary.matched_lines, 1000);
    assert_eq!(summary.stored_hits, 10);
}

#[test]
fn export_writes_matching_lines() {
    let file = TempFile::new(SAMPLE.as_bytes());
    let (hits, _) = search_blocking(&file.0, SearchOptions { chunk_size: 5, ..opts("alpha") }).unwrap();
    let out = TempFile::new(b"");
    let (cancel, done) = (Default::default(), AtomicU64::new(0));
    let n = hfr::export::export_hits(&file.0, &hits, &out.0, true, &cancel, &done).unwrap();
    assert_eq!(n, 5);
    let text = std::fs::read_to_string(&out.0).unwrap();
    assert_eq!(
        text,
        "1: alpha beta\n4: epsilon ALPHA\r\n5: zeta alphabet\n6:   indented alpha\n7: last line no newline alpha\n"
    );
}

#[test]
fn copy_lines_for_the_clipboard() {
    let file = TempFile::new(SAMPLE.as_bytes());
    let (hits, _) = search_blocking(&file.0, SearchOptions { chunk_size: 5, ..opts("alpha") }).unwrap();
    let (cancel, done) = (Default::default(), AtomicU64::new(0));
    let copy = |numbers, max_line, max_total| {
        hfr::export::copy_lines(&file.0, &hits, numbers, max_line, max_total, &cancel, &done).unwrap()
    };
    // Windows line breaks between lines, none after the last; "\r\n" in the file is not doubled.
    let all = "alpha beta\r\nepsilon ALPHA\r\nzeta alphabet\r\n  indented alpha\r\nlast line no newline alpha";
    assert_eq!(copy(false, 1 << 20, 1 << 20), (all.to_string(), 5));
    assert_eq!(done.load(Ordering::Relaxed), 5);
    let (text, n) = copy(true, 1 << 20, 1 << 20);
    assert_eq!(n, 5);
    assert!(text.starts_with("1: alpha beta\r\n4: epsilon ALPHA\r\n5: zeta"), "{text}");
    // Long lines are cut, and the total size is capped at whole lines.
    assert_eq!(copy(false, 5, 1 << 20).0, "alpha\r\nepsil\r\nzeta \r\n  ind\r\nlast ");
    assert_eq!(copy(false, 1 << 20, 30), ("alpha beta\r\nepsilon ALPHA".to_string(), 2));
    assert_eq!(copy(false, 1 << 20, 4), (String::new(), 0));
}

#[test]
fn bad_regex_is_reported() {
    let o = SearchOptions { regex: true, ..opts("(unclosed") };
    let err = build_regex("(unclosed", &o).unwrap_err();
    assert!(err.contains("Invalid regular expression"), "{err}");
    let err = Matcher::new(&o).unwrap_err();
    assert!(err.starts_with("Invalid regular expression"), "{err}");
    // With several texts, the message says which one is wrong.
    let err = Matcher::new(&SearchOptions { regex: true, ..SearchOptions::any_of(["ok", "(unclosed"]) }).unwrap_err();
    assert!(err.starts_with("Search text 2 (\"(unclosed\")"), "{err}");
    assert!(Matcher::new(&SearchOptions::any_of(["", ""])).is_err());
}

#[test]
fn any_of_several_texts() {
    let s = SAMPLE.as_bytes();
    check(s, SearchOptions::any_of(["beta", "zeta", "indented"]));
    // Overlapping texts: the leftmost match wins, then the earlier text.
    check(s, SearchOptions::any_of(["alphabet", "alpha"]));
    check(s, SearchOptions::any_of(["alpha", "alphabet"]));
    check(s, SearchOptions::any_of(["", "delta", "", "last"]));
    check(s, SearchOptions { whole_word: true, ..SearchOptions::any_of(["alpha", "gamma", "line"]) });
    check(s, SearchOptions { case_sensitive: true, ..SearchOptions::any_of(["ALPHA", "Gamma", "zeta"]) });
    check(s, SearchOptions { regex: true, ..SearchOptions::any_of([r"^$", r"(a)(l)p", r"(?P<x>n)o", r"\d"]) });
    check(s, SearchOptions { regex: true, ..SearchOptions::any_of([r"alpha$", r"^\s+"]) });
}

#[test]
fn all_of_several_texts() {
    let s = SAMPLE.as_bytes();
    check(s, SearchOptions::all_of(["alpha", "beta"]));
    check(s, SearchOptions::all_of(["beta", "alpha"]));
    check(s, SearchOptions::all_of(["alpha", "e"]));
    check(s, SearchOptions::all_of(["alpha", "no such text"]));
    check(s, SearchOptions::all_of(["", "alpha"]));
    check(s, SearchOptions { case_sensitive: true, ..SearchOptions::all_of(["ALPHA", "epsilon"]) });
    check(s, SearchOptions { whole_word: true, ..SearchOptions::all_of(["alpha", "zeta"]) });
    check(s, SearchOptions { regex: true, ..SearchOptions::all_of([r"^\w", r"a$", r"l+"]) });
    check(s, SearchOptions { regex: true, ..SearchOptions::all_of([r"^$", r"$"]) });
}

#[test]
fn several_texts_on_many_lines() {
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut s = Vec::new();
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        for k in 0..(x % 50) as usize {
            s.push(b"abcdefghij  ERROR\r"[(x as usize >> (k % 40)) % 18]);
        }
        s.push(b'\n');
    }
    check(&s, SearchOptions::any_of(["error", "abc", "jj"]));
    check(&s, SearchOptions::all_of(["error", "abc"]));
    check(&s, SearchOptions { regex: true, ..SearchOptions::all_of([r"^a", r"j\r?$"]) });
}

#[test]
fn highlights_tell_texts_apart() {
    let o = SearchOptions { regex: true, ..SearchOptions::any_of(["foo", r"(b)(a)r", "o"]) };
    let m = Matcher::new(&o).unwrap();
    assert_eq!(m.terms(), 3);
    assert_eq!(m.highlights(b"bar foo xo", 10), [(0..3, 1), (4..7, 0), (9..10, 2)]);
    assert_eq!(m.highlights(b"bar foo xo", 2).len(), 2);
    let single = Matcher::new(&opts("o")).unwrap();
    assert_eq!(single.highlights(b"foo", 10), [(1..2, 0), (2..3, 0)]);
}

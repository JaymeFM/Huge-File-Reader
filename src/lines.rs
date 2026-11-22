//! Random-access reading of lines: text windows for the viewer, match
//! snippets for the results list, and a sparse line index for "go to line".

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use memchr::{memchr, memrchr};

use crate::fileio::read_full_at;

/// Most bytes of a single line we ever keep for display.
pub const MAX_DISPLAY_BYTES: usize = 4096;
/// How far we look backwards/forwards for a line break before giving up.
const MAX_LINE_SCAN: u64 = 64 << 20;
const SCAN_BLOCK: usize = 1 << 20;

/// Number of newlines before each chunk boundary, i.e. `newlines_before[i]`
/// is the count of `\n` in bytes `[0, i * chunk_size)`. Built for free by a
/// search; may cover only a prefix of the file if a search was stopped early.
#[derive(Clone, Debug, Default)]
pub struct LineIndex {
    pub chunk_size: u64,
    pub file_len: u64,
    pub newlines_before: Vec<u64>,
}

impl LineIndex {
    /// Bytes from the start of the file whose newlines are counted.
    pub fn covered_bytes(&self) -> u64 {
        let chunks = self.newlines_before.len().saturating_sub(1) as u64;
        (chunks * self.chunk_size).min(self.file_len)
    }

    pub fn is_complete(&self) -> bool {
        self.chunk_size > 0 && self.covered_bytes() >= self.file_len
    }

    /// Keeps whichever index covers more of the file.
    pub fn merge_from(&mut self, other: &LineIndex) {
        let compatible = self.chunk_size == other.chunk_size && self.file_len == other.file_len;
        if !compatible || other.newlines_before.len() > self.newlines_before.len() {
            *self = other.clone();
        }
    }
}

/// One line as shown in the viewer.
#[derive(Clone, Debug)]
pub struct TextLine {
    /// Byte offset where the line starts.
    pub offset: u64,
    /// Full length of the line in bytes, without the line break.
    pub len: u64,
    /// Up to [`MAX_DISPLAY_BYTES`] of the line's raw bytes (no `\r\n`).
    pub bytes: Vec<u8>,
}

impl TextLine {
    pub fn is_cut(&self) -> bool {
        (self.bytes.len() as u64) < self.len
    }
    /// Offset of the first byte after this line's terminator.
    pub fn next_offset(&self, file_len: u64, terminator_len: u64) -> u64 {
        (self.offset + self.len + terminator_len).min(file_len)
    }
}

/// Start offset of the line that contains `offset`.
pub fn line_start_containing(file: &File, offset: u64) -> io::Result<u64> {
    let mut end = offset;
    let floor = offset.saturating_sub(MAX_LINE_SCAN);
    let mut buf = vec![0u8; 64 << 10];
    while end > floor {
        let start = end.saturating_sub(buf.len() as u64).max(floor);
        let len = (end - start) as usize;
        let n = read_full_at(file, &mut buf[..len], start)?;
        if let Some(p) = memrchr(b'\n', &buf[..n]) {
            return Ok(start + p as u64 + 1);
        }
        end = start;
    }
    Ok(floor)
}

/// Finds where the line starting at `start` ends. Returns
/// (length without terminator, terminator length).
fn line_extent(file: &File, file_len: u64, start: u64, buf: &mut Vec<u8>) -> io::Result<(u64, u64)> {
    let mut pos = start;
    buf.resize(SCAN_BLOCK, 0);
    while pos < file_len && pos - start < MAX_LINE_SCAN {
        let n = read_full_at(file, &mut buf[..], pos)?;
        if n == 0 {
            break;
        }
        if let Some(p) = memchr(b'\n', &buf[..n]) {
            let nl = pos + p as u64;
            let cr = nl > start && byte_at(file, nl - 1, buf, pos, n)? == b'\r';
            return Ok((nl - start - cr as u64, 1 + cr as u64));
        }
        pos += n as u64;
    }
    Ok(((pos.min(file_len)) - start, 0))
}

fn byte_at(file: &File, at: u64, buf: &[u8], buf_pos: u64, buf_len: usize) -> io::Result<u8> {
    if at >= buf_pos && at < buf_pos + buf_len as u64 {
        return Ok(buf[(at - buf_pos) as usize]);
    }
    let mut b = [0u8; 1];
    read_full_at(file, &mut b, at)?;
    Ok(b[0])
}

/// Reads up to `max_lines` lines starting at `start` (which must be a line
/// start). Returns the lines and the offset just after the last one.
pub fn read_lines_forward(
    file: &File,
    file_len: u64,
    start: u64,
    max_lines: usize,
) -> io::Result<(Vec<TextLine>, u64)> {
    const WINDOW: usize = 256 << 10;
    let mut out = Vec::new();
    let mut pos = start;
    let mut buf = vec![0u8; WINDOW];
    let mut scratch = Vec::new();
    'outer: while out.len() < max_lines && pos < file_len {
        let n = read_full_at(file, &mut buf, pos)?;
        if n == 0 {
            break;
        }
        let mut i = 0usize;
        while i < n && out.len() < max_lines {
            match memchr(b'\n', &buf[i..n]) {
                Some(p) => {
                    let mut line = &buf[i..i + p];
                    if line.last() == Some(&b'\r') {
                        line = &line[..line.len() - 1];
                    }
                    out.push(TextLine {
                        offset: pos + i as u64,
                        len: line.len() as u64,
                        bytes: line[..line.len().min(MAX_DISPLAY_BYTES)].to_vec(),
                    });
                    i += p + 1;
                }
                None if i == 0 || pos + n as u64 >= file_len => {
                    // A line longer than the window, or the unterminated last line.
                    let line_start = pos + i as u64;
                    let (len, term) = line_extent(file, file_len, line_start, &mut scratch)?;
                    let take = (n - i).min(MAX_DISPLAY_BYTES).min(len as usize);
                    out.push(TextLine { offset: line_start, len, bytes: buf[i..i + take].to_vec() });
                    pos = (line_start + len + term).min(file_len);
                    if term == 0 && pos < file_len {
                        pos = line_start + len; // gave up scanning a monstrous line
                    }
                    continue 'outer;
                }
                None => break, // partial line: re-read it at the start of the next window
            }
        }
        pos += i as u64;
    }
    Ok((out, pos))
}

/// Reads up to `max_lines` lines that end before `end` (a line start), in file order.
pub fn read_lines_backward(file: &File, file_len: u64, end: u64, max_lines: usize) -> io::Result<Vec<TextLine>> {
    let mut starts = Vec::new();
    let mut cursor = end;
    while cursor > 0 && starts.len() < max_lines {
        let s = line_start_containing(file, cursor - 1)?;
        starts.push(s);
        cursor = s;
    }
    let Some(&first) = starts.last() else { return Ok(Vec::new()) };
    let (mut lines, _) = read_lines_forward(file, file_len, first, starts.len())?;
    lines.retain(|l| l.offset < end);
    Ok(lines)
}

/// Reads the full line starting at `start` (without `\r\n`), up to `max` bytes.
pub fn read_line_bytes(file: &File, start: u64, max: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; SCAN_BLOCK.min(max.max(1))];
    let mut pos = start;
    while out.len() < max {
        let n = read_full_at(file, &mut buf, pos)?;
        if n == 0 {
            break;
        }
        if let Some(p) = memchr(b'\n', &buf[..n]) {
            out.extend_from_slice(&buf[..p]);
            break;
        }
        out.extend_from_slice(&buf[..n]);
        pos += n as u64;
    }
    out.truncate(max);
    if out.last() == Some(&b'\r') {
        out.pop();
    }
    Ok(out)
}

/// A short piece of a matching line, centred on the match.
#[derive(Clone, Debug, Default)]
pub struct Snippet {
    pub bytes: Vec<u8>,
    /// The snippet starts after the beginning of the line.
    pub cut_start: bool,
    /// The line continues after the snippet.
    pub cut_end: bool,
}

/// Reads a snippet of the line at `line_start` around `match_offset`.
pub fn read_snippet(file: &File, line_start: u64, match_offset: u64, before: usize, total: usize) -> io::Result<Snippet> {
    let start = match_offset.saturating_sub(before as u64).max(line_start);
    let mut buf = vec![0u8; total];
    let n = read_full_at(file, &mut buf, start)?;
    buf.truncate(n);
    let mut skip = 0;
    if start > line_start {
        // Don't start in the middle of a UTF-8 sequence.
        while skip < 3 && skip < buf.len() && (buf[skip] & 0xC0) == 0x80 {
            skip += 1;
        }
    }
    let (end, cut_end) = match memchr(b'\n', &buf) {
        Some(p) => (p, false),
        None => (buf.len(), n == total),
    };
    let mut end = end.max(skip);
    if end > skip && buf[end - 1] == b'\r' {
        end -= 1;
    }
    Ok(Snippet { bytes: buf[skip..end].to_vec(), cut_start: start > line_start, cut_end })
}

/// Line number of the line containing `offset`, if the index covers it.
pub fn line_number_at(file: &File, index: &LineIndex, offset: u64) -> io::Result<Option<u64>> {
    if index.chunk_size == 0 {
        return Ok(None);
    }
    let chunk = (offset / index.chunk_size) as usize;
    let Some(&before) = index.newlines_before.get(chunk) else { return Ok(None) };
    let start = chunk as u64 * index.chunk_size;
    let mut buf = vec![0u8; (offset - start) as usize];
    let n = read_full_at(file, &mut buf, start)?;
    Ok(Some(before + bytecount::count(&buf[..n], b'\n') as u64 + 1))
}

/// Finds the byte offset where 1-based line `line` starts. Uses (and
/// extends) `index`; scans the file only past what the index covers.
/// Returns `Ok(None)` if the file has fewer lines or the scan was cancelled.
pub fn find_line(
    file: &File,
    index: &mut LineIndex,
    line: u64,
    cancel: &AtomicBool,
    scanned: &AtomicU64,
) -> io::Result<Option<u64>> {
    if line <= 1 {
        return Ok(Some(0));
    }
    let target = line - 1; // the line starts right after this many newlines
    let csize = index.chunk_size.max(1);
    if index.newlines_before.is_empty() {
        index.newlines_before.push(0);
    }
    // Last chunk boundary with fewer than `target` newlines before it.
    let k = index.newlines_before.partition_point(|&n| n < target);
    let chunk = k.saturating_sub(1);
    let mut pos = chunk as u64 * csize;
    let mut seen = index.newlines_before[chunk];
    let mut buf = vec![0u8; csize as usize];
    while pos < index.file_len {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let len = (csize.min(index.file_len - pos)) as usize;
        let n = read_full_at(file, &mut buf[..len], pos)?;
        if n == 0 {
            break;
        }
        let here = bytecount::count(&buf[..n], b'\n') as u64;
        if seen + here >= target {
            let need = (target - seen) as usize;
            let p = memchr::memchr_iter(b'\n', &buf[..n]).nth(need - 1).expect("counted newline");
            let start = pos + p as u64 + 1;
            // A final "\n" doesn't begin another line.
            return Ok((start < index.file_len).then_some(start));
        }
        seen += here;
        pos += n as u64;
        scanned.store(pos, Ordering::Relaxed);
        // Record the boundary we just passed if the index didn't know it yet.
        let boundary = (pos / csize) as usize;
        if pos.is_multiple_of(csize) && boundary == index.newlines_before.len() {
            index.newlines_before.push(seen);
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(content: &[u8]) -> (tempdir::Dir, File, u64) {
        let dir = tempdir::Dir::new();
        let p = dir.path.join("t.txt");
        std::fs::File::create(&p).unwrap().write_all(content).unwrap();
        (dir, File::open(&p).unwrap(), content.len() as u64)
    }

    mod tempdir {
        pub struct Dir {
            pub path: std::path::PathBuf,
        }
        impl Dir {
            pub fn new() -> Self {
                use std::sync::atomic::{AtomicUsize, Ordering};
                static N: AtomicUsize = AtomicUsize::new(0);
                let path = std::env::temp_dir().join(format!(
                    "hfr-lines-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir { path }
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.path);
            }
        }
    }

    #[test]
    fn forward_and_backward() {
        let (_d, f, len) = temp_file(b"one\r\ntwo\nthree\nlast");
        let (lines, next) = read_lines_forward(&f, len, 0, 10).unwrap();
        let texts: Vec<_> = lines.iter().map(|l| String::from_utf8_lossy(&l.bytes).to_string()).collect();
        assert_eq!(texts, ["one", "two", "three", "last"]);
        assert_eq!(lines[1].offset, 5);
        assert_eq!(next, len);
        let back = read_lines_backward(&f, len, 14, 2).unwrap();
        let texts: Vec<_> = back.iter().map(|l| String::from_utf8_lossy(&l.bytes).to_string()).collect();
        assert_eq!(texts, ["two", "three"]);
        assert_eq!(line_start_containing(&f, 12).unwrap(), 9);
        assert_eq!(line_start_containing(&f, 2).unwrap(), 0);
    }

    #[test]
    fn goto_line_uses_and_extends_index() {
        let content: Vec<u8> = (1..=1000).flat_map(|i| format!("line {i}\n").into_bytes()).collect();
        let (_d, f, len) = temp_file(&content);
        let mut idx = LineIndex { chunk_size: 100, file_len: len, newlines_before: vec![0] };
        let (c, s) = (AtomicBool::new(false), AtomicU64::new(0));
        for target in [1u64, 2, 9, 10, 11, 500, 1000] {
            let off = find_line(&f, &mut idx, target, &c, &s).unwrap().unwrap();
            let (lines, _) = read_lines_forward(&f, len, off, 1).unwrap();
            assert_eq!(lines[0].bytes, format!("line {target}").into_bytes());
            assert_eq!(line_number_at(&f, &idx, off).unwrap(), Some(target));
        }
        assert!(find_line(&f, &mut idx, 1001, &c, &s).unwrap().is_none());
        assert!(idx.newlines_before.len() > 10);
    }

    #[test]
    fn snippet_centres_on_match() {
        let (_d, f, _) = temp_file(b"aaaaaaaaaaaaaaaaaaaaNEEDLEbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\nnext");
        let s = read_snippet(&f, 0, 20, 5, 30).unwrap();
        assert_eq!(s.bytes, b"aaaaaNEEDLEbbbbbbbbbbbbbbbbbbb");
        assert!(s.cut_start && s.cut_end);
        let s = read_snippet(&f, 0, 20, 5, 100).unwrap();
        assert!(!s.cut_end);
        assert!(s.bytes.ends_with(b"b"));
    }
}

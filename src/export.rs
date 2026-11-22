//! Writing matching lines out to a new file or the clipboard.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::search::Hit;

/// Calls `f` with each hit and its line from `src` (terminator included, if
/// any, and cut at `max_line` bytes), in order. Stops early when `f` returns
/// `false` or `cancel` is set. `done` tracks how many lines were handled.
/// Returns that count.
fn each_line(
    src: &Path,
    hits: &[Hit],
    max_line: u64,
    cancel: &AtomicBool,
    done: &AtomicU64,
    mut f: impl FnMut(&Hit, &[u8]) -> io::Result<bool>,
) -> io::Result<u64> {
    let mut reader = BufReader::with_capacity(4 << 20, File::open(src)?);
    let mut pos = 0u64; // current position of `reader`
    let mut line = Vec::new();
    let mut handled = 0u64;
    for hit in hits {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let delta = hit.line_start as i64 - pos as i64;
        if delta != 0 {
            if (0..(1 << 20)).contains(&delta) {
                reader.seek_relative(delta)?; // stays inside the buffer when it can
            } else {
                reader.seek(SeekFrom::Start(hit.line_start))?;
            }
        }
        line.clear();
        let n = reader.by_ref().take(max_line).read_until(b'\n', &mut line)?;
        pos = hit.line_start + n as u64;
        if !f(hit, &line)? {
            break;
        }
        handled += 1;
        if handled.is_multiple_of(4096) {
            done.store(handled, Ordering::Relaxed);
        }
    }
    done.store(handled, Ordering::Relaxed);
    Ok(handled)
}

/// Copies every hit's full line from `src` into `dest`, in file order,
/// optionally prefixed with `<line number>: `. Returns lines written.
/// `done` is updated with the number of lines written so far.
pub fn export_hits(
    src: &Path,
    hits: &[Hit],
    dest: &Path,
    with_line_numbers: bool,
    cancel: &AtomicBool,
    done: &AtomicU64,
) -> io::Result<u64> {
    let mut out = BufWriter::with_capacity(4 << 20, File::create(dest)?);
    let written = each_line(src, hits, u64::MAX, cancel, done, |hit, line| {
        if with_line_numbers {
            write!(out, "{}: ", hit.line_no)?;
        }
        out.write_all(line)?;
        if line.last() != Some(&b'\n') {
            out.write_all(b"\n")?;
        }
        Ok(true)
    })?;
    out.flush()?;
    Ok(written)
}

/// The hits' lines as one text for the clipboard: lines joined with "\r\n",
/// each cut at `max_line` bytes, optionally prefixed with `<line number>: `.
/// Stops before the text would grow past `max_total` bytes. Returns the text
/// and how many lines it holds.
pub fn copy_lines(
    src: &Path,
    hits: &[Hit],
    with_line_numbers: bool,
    max_line: usize,
    max_total: usize,
    cancel: &AtomicBool,
    done: &AtomicU64,
) -> io::Result<(String, u64)> {
    let mut text = Vec::new();
    let mut first = true;
    let lines = each_line(src, hits, max_line as u64 + 2, cancel, done, |hit, line| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = &line[..line.len().min(max_line)];
        let prefix = if with_line_numbers { format!("{}: ", hit.line_no) } else { String::new() };
        let sep = if first { 0 } else { 2 };
        first = false;
        if text.len() + sep + prefix.len() + line.len() > max_total {
            return Ok(false);
        }
        if sep > 0 {
            text.extend_from_slice(b"\r\n");
        }
        text.extend_from_slice(prefix.as_bytes());
        text.extend_from_slice(line);
        Ok(true)
    })?;
    let text = match String::from_utf8(text) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    Ok((text, lines))
}

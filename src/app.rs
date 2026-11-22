//! The desktop UI: a search bar, a virtualized results list and a viewer
//! that shows any part of the file on demand.

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use eframe::egui::{
    self,
    text::{CCursor, CCursorRange, LayoutJob},
    Align, Color32, FontId, Key, KeyboardShortcut, Layout, Modifiers, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextFormat, TextStyle, Vec2,
};

use hfr::fileio::{detect_drive_kind, open_read, read_full_at, DriveKind};
use hfr::fmt;
use hfr::lines::{self, LineIndex, Snippet, TextLine};
use hfr::search::{self, Hit, Matcher, SearchEvent, SearchHandle, SearchOptions, SearchSummary};

/// Lines loaded into the viewer per step when scrolling past an edge.
const VIEW_STEP_LINES: usize = 300;
/// Lines of context loaded above a jump target.
const VIEW_CONTEXT_ABOVE: usize = 100;
/// The viewer keeps at most this many lines in memory.
const VIEW_MAX_LINES: usize = 6000;
const SNIPPET_BEFORE: usize = 80;
const SNIPPET_TOTAL: usize = 600;
/// Most bytes copied from a single line.
const COPY_MAX: usize = 16 << 20;
/// Most text put on the clipboard at once.
const CLIPBOARD_MAX: usize = 64 << 20;
/// The search box grows to this many rows, then scrolls.
const SEARCH_BOX_ROWS: f32 = 6.0;
/// Highlight colors for the search texts, in order (dark text on top).
const TERM_COLORS: [Color32; 6] = [
    Color32::from_rgb(255, 204, 0),
    Color32::from_rgb(110, 200, 255),
    Color32::from_rgb(140, 225, 110),
    Color32::from_rgb(255, 150, 200),
    Color32::from_rgb(255, 165, 80),
    Color32::from_rgb(190, 165, 255),
];

fn term_color(term: usize) -> Color32 {
    TERM_COLORS[term % TERM_COLORS.len()]
}

fn search_box_id() -> egui::Id {
    egui::Id::unique("search-box")
}

/// The search box holds one search text per line.
fn search_terms(query: &str) -> Vec<String> {
    query
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;

struct OpenFile {
    path: PathBuf,
    name: String,
    len: u64,
    /// Handle used by the UI thread for small reads (viewer, snippets).
    file: File,
    drive: DriveKind,
    warning: Option<String>,
    index: LineIndex,
    total_lines: Option<u64>,
}

#[derive(Default)]
struct Viewer {
    lines: Vec<TextLine>,
    /// Offset of the first loaded line.
    start: u64,
    /// Offset just after the last loaded line.
    end: u64,
    first_line_no: Option<u64>,
    /// Offset of the marked line (the selected hit or jump target).
    current: Option<u64>,
    scroll_to_row: Option<usize>,
    /// Pixels to add to the scroll offset next frame (after loading above).
    scroll_adjust: f32,
    last_offset_y: f32,
    /// Offset of the top visible line, for the position slider.
    top_offset: u64,
}

struct GotoJob {
    line: u64,
    cancel: Arc<AtomicBool>,
    scanned: Arc<AtomicU64>,
    rx: Receiver<(Result<Option<u64>, String>, LineIndex)>,
}

struct ExportJob {
    dest: PathBuf,
    total: u64,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    rx: Receiver<Result<u64, String>>,
}

struct CopyJob {
    /// Lines asked for, and the first one's number.
    lines: u64,
    first_line_no: u64,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicU64>,
    rx: Receiver<Result<(String, u64), String>>,
}

struct Status {
    text: String,
    error: bool,
}

enum ResultAction {
    Copy { with_line_numbers: bool },
    SelectAll,
}

pub struct App {
    file: Option<OpenFile>,

    /// The search texts, one per line.
    query: String,
    use_regex: bool,
    match_case: bool,
    whole_word: bool,
    /// With several search texts: lines must contain all of them.
    match_all: bool,
    search_cursor_to_end: bool,

    search: Option<SearchHandle>,
    summary: Option<SearchSummary>,
    highlight: Option<Matcher>,
    hits: Arc<Vec<Hit>>,
    /// The current result, shown in the viewer.
    selected: Option<usize>,
    /// Where Shift+click / Shift+arrow selections start from.
    sel_anchor: usize,
    /// Selected results (inclusive) when more than the current one is
    /// selected. The end may be `usize::MAX`, meaning "to the end of the list".
    sel_range: Option<(usize, usize)>,
    /// Ctrl+C copies the marked viewer line instead of the selected results.
    copy_from_viewer: bool,
    copy: Option<CopyJob>,
    snippets: HashMap<usize, Snippet>,
    results_scroll_to: Option<usize>,
    results_visible: Range<usize>,

    viewer: Viewer,
    goto_text: String,
    jump_pct: f32,
    slider_active: bool,
    goto: Option<GotoJob>,

    export: Option<ExportJob>,
    export_line_numbers: bool,

    status: Option<Status>,
    focus_search: bool,
    pending_query: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, file: Option<PathBuf>, query: Option<String>) -> Self {
        install_fallback_fonts(&cc.egui_ctx);
        let mut app = App {
            file: None,
            query: String::new(),
            use_regex: false,
            match_case: false,
            whole_word: false,
            match_all: false,
            search_cursor_to_end: false,
            search: None,
            summary: None,
            highlight: None,
            hits: Arc::new(Vec::new()),
            selected: None,
            sel_anchor: 0,
            sel_range: None,
            copy_from_viewer: false,
            copy: None,
            snippets: HashMap::new(),
            results_scroll_to: None,
            results_visible: 0..0,
            viewer: Viewer::default(),
            goto_text: String::new(),
            jump_pct: 0.0,
            slider_active: false,
            goto: None,
            export: None,
            export_line_numbers: false,
            status: None,
            focus_search: true,
            pending_query: None,
        };
        if let Some(p) = file {
            app.open_file(&cc.egui_ctx, p);
            if let Some(q) = query {
                app.query = q.clone();
                app.pending_query = Some(q);
                // Like pressing Enter: the results list gets the keyboard.
                app.focus_search = false;
            }
        }
        app
    }

    // ----------------------------------------------------------------- actions

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status { text: text.into(), error });
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new().set_title("Open a text file");
        if let Some(dir) = self.file.as_ref().and_then(|f| f.path.parent()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.open_file(ctx, path);
        }
    }

    fn open_file(&mut self, ctx: &egui::Context, path: PathBuf) {
        let opened = File::open(&path).and_then(|f| {
            let meta = f.metadata()?;
            if meta.is_dir() {
                return Err(std::io::Error::other("that is a folder, not a file"));
            }
            Ok((f, meta.len()))
        });
        let (file, len) = match opened {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("Could not open {}: {e}", path.display()), true);
                return;
            }
        };
        self.stop_search();
        self.goto = None;
        self.export = None;
        self.reset_results();
        self.summary = None;
        let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        let warning = sniff_encoding(&file, len);
        let drive = detect_drive_kind(&path);
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — Huge File Reader")));
        self.file = Some(OpenFile {
            path,
            name,
            len,
            file,
            drive,
            warning,
            index: LineIndex::default(),
            total_lines: None,
        });
        self.status = None;
        self.view_at(0, Some(1));
        self.focus_search = true;
    }

    fn reset_results(&mut self) {
        self.hits = Arc::new(Vec::new());
        self.selected = None;
        self.sel_anchor = 0;
        self.sel_range = None;
        if let Some(job) = self.copy.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.snippets.clear();
        self.results_scroll_to = None;
        self.results_visible = 0..0;
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        let Some(f) = &self.file else { return };
        let opts = SearchOptions {
            regex: self.use_regex,
            case_sensitive: self.match_case,
            whole_word: self.whole_word,
            match_all: self.match_all,
            io_parallelism: f.drive.io_parallelism(),
            ..SearchOptions::any_of(search_terms(&self.query))
        };
        let regex = match Matcher::new(&opts) {
            Ok(r) => r,
            Err(e) => {
                self.set_status(e, true);
                return;
            }
        };
        let path = f.path.clone();
        self.stop_search();
        self.reset_results();
        self.summary = None;
        let waker = ctx.clone();
        match search::start_search(&path, opts, Some(Arc::new(move || waker.request_repaint()))) {
            Ok(handle) => {
                self.highlight = Some(regex);
                self.search = Some(handle);
                self.status = None;
            }
            Err(e) => self.set_status(e, true),
        }
    }

    fn stop_search(&mut self) {
        if let Some(h) = &self.search {
            h.cancel();
        }
        // Keep the handle until its Finished event arrives so we get a summary.
    }

    /// Makes result `i` the current one and the only one selected.
    fn select_hit(&mut self, i: usize) {
        let Some(&hit) = self.hits.get(i) else { return };
        self.selected = Some(i);
        self.sel_anchor = i;
        self.sel_range = None;
        self.copy_from_viewer = false;
        self.results_scroll_to = Some(i);
        self.view_at(hit.line_start, Some(hit.line_no));
    }

    /// Selects everything from the anchor to result `i`, which becomes current.
    fn extend_selection(&mut self, i: usize) {
        let Some(&hit) = self.hits.get(i) else { return };
        let anchor = self.sel_anchor.min(self.hits.len() - 1);
        self.selected = Some(i);
        self.sel_range = Some((anchor.min(i), anchor.max(i)));
        self.copy_from_viewer = false;
        self.results_scroll_to = Some(i);
        self.view_at(hit.line_start, Some(hit.line_no));
    }

    /// Selects every result, including ones a running search adds later.
    fn select_all(&mut self) {
        if self.hits.is_empty() {
            return;
        }
        self.sel_range = Some((0, usize::MAX));
        self.sel_anchor = 0;
        self.copy_from_viewer = false;
    }

    /// The selected results as an inclusive range, clamped to the list.
    fn selection(&self) -> Option<(usize, usize)> {
        let last = self.hits.len().checked_sub(1)?;
        match self.sel_range {
            Some((a, b)) => Some((a.min(last), b.min(last))),
            None => self.selected.filter(|&i| i <= last).map(|i| (i, i)),
        }
    }

    fn selected_count(&self) -> usize {
        self.selection().map_or(0, |(a, b)| b - a + 1)
    }

    fn move_selection(&mut self, delta: isize, extend: bool) {
        let n = self.hits.len();
        if n == 0 {
            return;
        }
        let next = match self.selected {
            None if delta >= 0 => 0,
            None => n - 1,
            Some(i) => (i as isize + delta).clamp(0, n as isize - 1) as usize,
        };
        if extend {
            self.extend_selection(next);
        } else if Some(next) != self.selected || self.sel_range.is_some() {
            self.select_hit(next);
        }
    }

    /// Copies the selected results' full lines to the clipboard, in the
    /// background (they may be spread over a huge file).
    fn copy_selected(&mut self, ctx: &egui::Context, with_line_numbers: bool) {
        let (Some(f), Some((lo, hi))) = (&self.file, self.selection()) else { return };
        if self.copy.is_some() {
            return;
        }
        let (src, hits) = (f.path.clone(), Arc::clone(&self.hits));
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (c, d, waker) = (Arc::clone(&cancel), Arc::clone(&done), ctx.clone());
        std::thread::spawn(move || {
            let r = hfr::export::copy_lines(&src, &hits[lo..=hi], with_line_numbers, COPY_MAX, CLIPBOARD_MAX, &c, &d);
            let _ = tx.send(r.map_err(|e| e.to_string()));
            waker.request_repaint();
        });
        let first_line_no = self.hits[lo].line_no;
        self.copy = Some(CopyJob { lines: (hi - lo + 1) as u64, first_line_no, cancel, done, rx });
    }

    /// Line number of the marked viewer line, if known.
    fn viewer_current_line_no(&self) -> Option<u64> {
        let v = &self.viewer;
        let cur = v.current?;
        let idx = v.lines.partition_point(|l| l.offset < cur);
        (idx < v.lines.len()).then_some(v.first_line_no? + idx as u64)
    }

    /// Loads the viewer around `offset` and marks that line.
    fn view_at(&mut self, offset: u64, line_no: Option<u64>) {
        let Some(f) = &self.file else { return };
        match load_view(f, offset, line_no) {
            Ok(v) => self.viewer = v,
            Err(e) => self.set_status(format!("Could not read the file: {e}"), true),
        }
    }

    fn viewer_load_above(&mut self, stride: f32) {
        let Some(f) = &self.file else { return };
        let v = &mut self.viewer;
        match lines::read_lines_backward(&f.file, f.len, v.start, VIEW_STEP_LINES) {
            Ok(before) if !before.is_empty() => {
                let k = before.len();
                v.start = before[0].offset;
                v.lines.splice(0..0, before);
                v.first_line_no = v.first_line_no.map(|n| n.saturating_sub(k as u64));
                v.scroll_adjust += k as f32 * stride;
                if v.lines.len() > VIEW_MAX_LINES {
                    v.end = v.lines[VIEW_MAX_LINES].offset;
                    v.lines.truncate(VIEW_MAX_LINES);
                }
            }
            Ok(_) => v.start = 0,
            Err(e) => self.status = Some(Status { text: format!("Could not read the file: {e}"), error: true }),
        }
    }

    fn viewer_load_below(&mut self, stride: f32) {
        let Some(f) = &self.file else { return };
        let v = &mut self.viewer;
        match lines::read_lines_forward(&f.file, f.len, v.end, VIEW_STEP_LINES) {
            Ok((more, end)) => {
                v.end = if more.is_empty() { f.len } else { end };
                v.lines.extend(more);
                if v.lines.len() > VIEW_MAX_LINES {
                    let drop = v.lines.len() - VIEW_MAX_LINES;
                    v.lines.drain(..drop);
                    v.start = v.lines[0].offset;
                    v.first_line_no = v.first_line_no.map(|n| n + drop as u64);
                    v.scroll_adjust -= drop as f32 * stride;
                }
            }
            Err(e) => self.status = Some(Status { text: format!("Could not read the file: {e}"), error: true }),
        }
    }

    fn start_goto(&mut self, ctx: &egui::Context) {
        let Some(f) = &self.file else { return };
        let cleaned: String = self.goto_text.chars().filter(|c| c.is_ascii_digit()).collect();
        let line = match cleaned.parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => {
                self.set_status("Type a line number (1 or more) to go to.", true);
                return;
            }
        };
        let handle = match open_read(&f.path, true) {
            Ok(h) => h,
            Err(e) => {
                self.set_status(format!("Could not open the file: {e}"), true);
                return;
            }
        };
        let mut index = f.index.clone();
        if index.chunk_size == 0 {
            index = LineIndex {
                chunk_size: search::DEFAULT_CHUNK_SIZE as u64,
                file_len: f.len,
                newlines_before: vec![0],
            };
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let scanned = Arc::new(AtomicU64::new(index.covered_bytes()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (c, s, waker) = (Arc::clone(&cancel), Arc::clone(&scanned), ctx.clone());
        std::thread::spawn(move || {
            let r = lines::find_line(&handle, &mut index, line, &c, &s).map_err(|e| e.to_string());
            let _ = tx.send((r, index));
            waker.request_repaint();
        });
        self.goto = Some(GotoJob { line, cancel, scanned, rx });
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let Some(f) = &self.file else { return };
        let stem = Path::new(&f.name).file_stem().map_or("results".into(), |s| s.to_string_lossy().into_owned());
        let Some(dest) = rfd::FileDialog::new()
            .set_title("Save matching lines")
            .set_file_name(format!("{stem}-matches.txt"))
            .save_file()
        else {
            return;
        };
        if dest == f.path {
            self.set_status("Choose a different file than the one being searched.", true);
            return;
        }
        let (src, hits, numbers) = (f.path.clone(), Arc::clone(&self.hits), self.export_line_numbers);
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicU64::new(0));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let (c, d, out, waker) = (Arc::clone(&cancel), Arc::clone(&done), dest.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = hfr::export::export_hits(&src, &hits, &out, numbers, &c, &d).map_err(|e| e.to_string());
            let _ = tx.send(r);
            waker.request_repaint();
        });
        self.export = Some(ExportJob { dest, total: self.hits.len() as u64, cancel, done, rx });
    }

    fn copy_line(&mut self, ctx: &egui::Context, offset: u64, line_no: Option<u64>) {
        let Some(f) = &self.file else { return };
        match lines::read_line_bytes(&f.file, offset, COPY_MAX) {
            Ok(bytes) => {
                ctx.copy_text(String::from_utf8_lossy(&bytes).into_owned());
                let what = line_no.map_or("Line".to_string(), |n| format!("Line {}", fmt::count(n)));
                let note = if bytes.len() >= COPY_MAX { " (first 16 MB)" } else { "" };
                self.set_status(format!("{what} copied to the clipboard{note}."), false);
            }
            Err(e) => self.set_status(format!("Could not read the line: {e}"), true),
        }
    }

    // ------------------------------------------------------------ background

    fn poll_search(&mut self) {
        let Some(h) = &self.search else { return };
        let mut finished = None;
        let mut first_batch = false;
        while let Ok(ev) = h.events.try_recv() {
            match ev {
                SearchEvent::Hits(batch) => {
                    first_batch |= self.hits.is_empty();
                    Arc::make_mut(&mut self.hits).extend(batch);
                }
                SearchEvent::Finished(s) => finished = Some(s),
            }
        }
        if first_batch && self.selected.is_none() {
            self.select_hit(0);
        }
        if let Some(s) = finished {
            self.search = None;
            if let Some(f) = &mut self.file {
                f.index.merge_from(&s.line_index);
                if s.total_lines.is_some() {
                    f.total_lines = s.total_lines;
                }
                // The viewer may now be able to show line numbers.
                if self.viewer.first_line_no.is_none() && f.index.covered_bytes() > self.viewer.start {
                    if let Ok(n) = lines::line_number_at(&f.file, &f.index, self.viewer.start) {
                        self.viewer.first_line_no = n;
                    }
                }
            }
            if let Some(e) = &s.error {
                self.status = Some(Status { text: format!("Search stopped: {e}"), error: true });
            } else if s.truncated_long_lines {
                self.status = Some(Status {
                    text: "Some lines are longer than 256 MB; only their first 256 MB were searched.".into(),
                    error: true,
                });
            }
            self.summary = Some(s);
        }
    }

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(job) = &self.copy {
            if let Ok(result) = job.rx.try_recv() {
                let (wanted, first, cancelled) = (job.lines, job.first_line_no, job.cancel.load(Ordering::Relaxed));
                self.copy = None;
                match result {
                    Ok(_) if cancelled => self.set_status("Copy cancelled.", false),
                    Ok((text, n)) => {
                        ctx.copy_text(text);
                        let msg = if n == wanted && n == 1 {
                            format!("Line {} copied to the clipboard.", fmt::count(first))
                        } else if n == wanted {
                            format!("{} lines copied to the clipboard.", fmt::count(n))
                        } else {
                            format!(
                                "Copied the first {} of {} lines: the clipboard takes up to {}. \
                                 Use “Save matching lines…” to get them all.",
                                fmt::count(n),
                                fmt::count(wanted),
                                fmt::bytes(CLIPBOARD_MAX as u64)
                            )
                        };
                        self.set_status(msg, n != wanted);
                    }
                    Err(e) => self.set_status(format!("Could not copy: {e}"), true),
                }
            }
        }
        if let Some(job) = &self.goto {
            if let Ok((result, index)) = job.rx.try_recv() {
                let (line, cancelled) = (job.line, job.cancel.load(Ordering::Relaxed));
                self.goto = None;
                if let Some(f) = &mut self.file {
                    f.index.merge_from(&index);
                }
                match result {
                    Ok(Some(offset)) => {
                        self.view_at(offset, Some(line));
                        self.status = None;
                    }
                    Ok(None) if cancelled => {}
                    Ok(None) => {
                        let total = self.file.as_ref().and_then(|f| f.total_lines);
                        let msg = match total {
                            Some(t) => format!("The file has only {} lines.", fmt::count(t)),
                            None => format!("The file has fewer than {} lines.", fmt::count(line)),
                        };
                        self.set_status(msg, true);
                    }
                    Err(e) => self.set_status(format!("Could not go to line {line}: {e}"), true),
                }
            }
        }
        if let Some(job) = &self.export {
            if let Ok(result) = job.rx.try_recv() {
                let dest = job.dest.display().to_string();
                self.export = None;
                match result {
                    Ok(n) => self.set_status(format!("Saved {} lines to {dest}", fmt::count(n)), false),
                    Err(e) => self.set_status(format!("Export failed: {e}"), true),
                }
            }
        }
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_file(ctx, p);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::O)) {
            self.pick_file(ctx);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
            self.focus_search = true;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::G)) {
            ctx.memory_mut(|m| m.request_focus(egui::Id::unique("goto-line")));
        }
        if ctx.input_mut(|i| i.key_pressed(Key::Escape)) {
            if let Some(h) = &self.search {
                h.cancel();
            } else if let Some(job) = &self.copy {
                job.cancel.store(true, Ordering::Relaxed);
            }
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::F3)) {
            self.move_selection(-1, false);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F3)) {
            self.move_selection(1, false);
        }
        if self.file.is_none() || ctx.egui_wants_keyboard_input() {
            return;
        }
        // Nothing is being typed into: keys act on the results list.
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
            self.select_all();
        }
        let copy = ctx.input_mut(|i| {
            let before = i.events.len();
            i.events.retain(|e| !matches!(e, egui::Event::Copy));
            i.events.len() != before
        });
        if copy {
            match self.viewer.current.filter(|_| self.copy_from_viewer) {
                Some(offset) => self.copy_line(ctx, offset, self.viewer_current_line_no()),
                None => self.copy_selected(ctx, false),
            }
        }
        let page = self.results_visible.len().saturating_sub(2).max(1) as isize;
        let moves = [
            (Key::ArrowDown, 1),
            (Key::ArrowUp, -1),
            (Key::PageDown, page),
            (Key::PageUp, -page),
            (Key::Home, isize::MIN / 2),
            (Key::End, isize::MAX / 2),
        ];
        for (key, delta) in moves {
            // Check Shift first: the plain shortcut would also match Shift+key.
            if ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, key)) {
                self.move_selection(delta, true);
            }
            if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, key)) {
                self.move_selection(delta, false);
            }
        }
        // Typing (or pasting) goes to the search box.
        let typing = ctx.input(|i| {
            i.events.iter().any(|e| match e {
                egui::Event::Text(t) | egui::Event::Paste(t) => !t.is_empty(),
                egui::Event::Key { key: Key::Backspace, pressed: true, .. } => true,
                _ => false,
            })
        });
        if typing {
            ctx.memory_mut(|m| m.request_focus(search_box_id()));
        }
    }

    // ---------------------------------------------------------------- panels

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("📂 Open file…").on_hover_text("Ctrl+O, or drop a file on the window").clicked() {
                self.pick_file(&ctx);
            }
            match &self.file {
                Some(f) => {
                    ui.label(RichText::new(&f.name).strong()).on_hover_text(f.path.display().to_string());
                    let mut info = format!("{}  ·  {}", fmt::bytes(f.len), f.drive.label());
                    if let Some(n) = f.total_lines {
                        info.push_str(&format!("  ·  {} lines", fmt::count(n)));
                    }
                    let how = match f.drive {
                        DriveKind::Ssd => "On an SSD the file is read in several places at once.",
                        _ => "The file is read strictly front to back, which is fastest for hard drives.",
                    };
                    ui.weak(info).on_hover_text(how);
                }
                None => {
                    ui.weak("No file open");
                }
            }
        });
        if let Some(w) = self.file.as_ref().and_then(|f| f.warning.clone()) {
            ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {w}"));
        }
        ui.add_space(2.0);

        let has_file = self.file.is_some();
        let n_terms = search_terms(&self.query).len();
        let mut go = false;
        ui.horizontal_top(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                if self.search.is_some() {
                    if ui.add_sized([96.0, 24.0], egui::Button::new("■  Stop")).on_hover_text("Esc").clicked() {
                        self.stop_search();
                    }
                } else {
                    let can = has_file && n_terms > 0;
                    go |= ui.add_enabled(can, egui::Button::new("🔍  Search").min_size(Vec2::new(96.0, 24.0))).clicked();
                }
                let add = egui::Button::new("➕").min_size(Vec2::new(28.0, 24.0));
                if ui.add_enabled(has_file, add).on_hover_text("Add another search text (Shift+Enter)").clicked() {
                    if !self.query.is_empty() && !self.query.ends_with('\n') {
                        self.query.push('\n');
                    }
                    self.focus_search = true;
                    self.search_cursor_to_end = true;
                }

                let id = search_box_id();
                if std::mem::take(&mut self.search_cursor_to_end) {
                    let mut state = egui::text_edit::TextEditState::load(&ctx, id).unwrap_or_default();
                    let end = CCursor::new(self.query.chars().count());
                    state.cursor.set_char_range(Some(CCursorRange::one(end)));
                    state.store(&ctx, id);
                }
                let font = TextStyle::Monospace.resolve(ui.style());
                let max_h = ui.fonts_mut(|f| f.row_height(&font)) * SEARCH_BOX_ROWS + 8.0;
                let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
                    let mut job = search_box_layout(text.as_str(), &font, ui.visuals().text_color());
                    job.wrap.max_width = wrap_width;
                    ui.fonts_mut(|f| f.layout_job(job))
                };
                let hint = if has_file {
                    "Search for text… (Enter to search, Shift+Enter to add another text)"
                } else {
                    "Open a file first"
                };
                let resp = ScrollArea::vertical()
                    .id_salt("search-box-scroll")
                    .max_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let edit = egui::TextEdit::multiline(&mut self.query)
                            .id(id)
                            .hint_text(hint)
                            .font(TextStyle::Monospace)
                            .desired_rows(1)
                            .desired_width(f32::INFINITY)
                            .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                            .layouter(&mut layouter);
                        ui.add_enabled(has_file, edit)
                    })
                    .inner;
                if self.focus_search && has_file {
                    resp.request_focus();
                    self.focus_search = false;
                }
                // Enter searches (Shift+Enter starts a new line). Focus then
                // moves to the results, so their keyboard shortcuts work.
                if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift) && n_terms > 0 {
                    go = true;
                    resp.surrender_focus();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.toggle_value(&mut self.match_case, "Aa Match case").on_hover_text("Upper/lower case must match");
            ui.toggle_value(&mut self.whole_word, "Whole word").on_hover_text("Only match complete words");
            ui.toggle_value(&mut self.use_regex, ".* Regex")
                .on_hover_text("Treat the search texts as regular expressions, e.g.  error|fail  or  ^\\d{4}-");
            ui.separator();
            if n_terms > 1 {
                ui.label(format!("{n_terms} search texts. Show lines with"));
                ui.radio_value(&mut self.match_all, false, "any of them");
                ui.radio_value(&mut self.match_all, true, "all of them");
            } else if has_file {
                ui.weak("To search for several texts, put each on its own line (Shift+Enter or ➕).");
            }
        });
        if let Some(q) = self.pending_query.take() {
            self.query = q;
            go = true;
        }
        if go {
            self.start_search(&ctx);
        }

        ui.add_space(2.0);
        self.progress_line(ui);
        ui.add_space(4.0);
    }

    fn progress_line(&mut self, ui: &mut egui::Ui) {
        if let Some(h) = &self.search {
            let p = &h.progress;
            let done = p.bytes_done.load(Ordering::Relaxed);
            let total = p.total_bytes.max(1);
            let matched = p.matched_lines.load(Ordering::Relaxed);
            let elapsed = p.started.elapsed();
            let speed = done as f64 / elapsed.as_secs_f64().max(0.001);
            let left = if done > 0 && speed > 0.0 {
                fmt::duration(Duration::from_secs_f64((total - done.min(total)) as f64 / speed)) + " left"
            } else {
                "starting…".into()
            };
            let text = format!(
                "{:.1}%   ·   {} of {}   ·   {}/s   ·   {}   ·   {} matching lines",
                done as f64 * 100.0 / total as f64,
                fmt::bytes(done),
                fmt::bytes(total),
                fmt::bytes(speed as u64),
                left,
                fmt::count(matched)
            );
            ui.add(egui::ProgressBar::new(done as f32 / total as f32).text(text).desired_height(20.0));
        } else if let Some(s) = &self.summary {
            let speed = s.bytes_searched as f64 / s.elapsed.as_secs_f64().max(0.001);
            let lines = if s.matched_lines == 1 { "line" } else { "lines" };
            let text = if s.cancelled || s.error.is_some() {
                format!(
                    "Stopped after {} ({}): {} matching {lines} so far.",
                    fmt::bytes(s.bytes_searched),
                    fmt::duration(s.elapsed),
                    fmt::count(s.matched_lines)
                )
            } else {
                format!(
                    "Done: {} matching {lines}. Searched {} in {} ({}/s).",
                    fmt::count(s.matched_lines),
                    fmt::bytes(s.bytes_searched),
                    fmt::duration(s.elapsed),
                    fmt::bytes(speed as u64)
                )
            };
            ui.label(text);
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match &self.status {
                Some(s) if s.error => {
                    ui.colored_label(ui.visuals().error_fg_color, &s.text);
                }
                Some(s) => {
                    ui.label(&s.text);
                }
                None => {}
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.weak(
                    "Ctrl+O open  ·  Ctrl+F search  ·  F3 / ↑↓ next match  ·  Ctrl+A select all  ·  Ctrl+C copy  ·  \
                     Ctrl+G go to line  ·  Esc stop",
                );
            });
        });
    }

    fn welcome(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading("Huge File Reader");
            ui.add_space(8.0);
            ui.label("Search and browse text files of any size. Even 100 GB+ files open instantly;");
            ui.label("nothing is loaded into memory, the file is only streamed while searching.");
            ui.add_space(16.0);
            if ui.add(egui::Button::new(RichText::new("📂  Open a file…").size(16.0)).min_size(Vec2::new(180.0, 36.0))).clicked() {
                self.pick_file(&ctx);
            }
            ui.add_space(8.0);
            ui.weak("…or drag and drop a file onto this window.");
        });
    }

    fn results_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let stored = self.hits.len() as u64;
            let matched = match (&self.search, &self.summary) {
                (Some(h), _) => h.progress.matched_lines.load(Ordering::Relaxed),
                (None, Some(s)) => s.matched_lines,
                _ => stored,
            };
            if self.search.is_none() && self.summary.is_none() {
                ui.weak("Type what you're looking for above and press Enter.");
            } else if matched == 0 {
                ui.label(if self.search.is_some() { "No matches yet…" } else { "No matches found." });
            } else {
                ui.label(RichText::new(format!("{} matching lines", fmt::count(matched))).strong());
                if matched > stored && self.search.is_none() {
                    ui.weak(format!("(listing the first {})", fmt::count(stored)));
                }
                let n_sel = self.selected_count();
                if n_sel > 1 {
                    ui.label(format!("·  {} selected", fmt::count(n_sel as u64)));
                    ui.weak("Ctrl+C copies them");
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(job) = &self.copy {
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                    let frac = job.done.load(Ordering::Relaxed) as f32 / job.lines.max(1) as f32;
                    ui.add(egui::ProgressBar::new(frac).desired_width(160.0).show_percentage());
                    ui.label("Copying…");
                } else if let Some(job) = &self.export {
                    if ui.button("Cancel").clicked() {
                        job.cancel.store(true, Ordering::Relaxed);
                    }
                    let frac = job.done.load(Ordering::Relaxed) as f32 / job.total.max(1) as f32;
                    ui.add(egui::ProgressBar::new(frac).desired_width(160.0).show_percentage());
                    ui.label("Saving…");
                } else {
                    let can = !self.hits.is_empty() && self.search.is_none();
                    if ui
                        .add_enabled(can, egui::Button::new("💾 Save matching lines…"))
                        .on_hover_text("Write every listed matching line to a new text file")
                        .clicked()
                    {
                        self.start_export(&ctx);
                    }
                    ui.checkbox(&mut self.export_line_numbers, "with line numbers");
                }
            });
        });
        ui.separator();

        let font = TextStyle::Monospace.resolve(ui.style());
        let row_h = ui.text_style_height(&TextStyle::Monospace) + 4.0;
        let stride = row_h + ui.spacing().item_spacing.y;
        let mut area = ScrollArea::vertical().id_salt("results").auto_shrink([false, false]);
        if let Some(row) = self.results_scroll_to.take() {
            let vis = &self.results_visible;
            let comfortably_visible = vis.len() > 4 && row > vis.start + 1 && row + 2 < vis.end;
            if !comfortably_visible {
                let view_h = ui.available_height();
                area = area.vertical_scroll_offset((row as f32 * stride - view_h * 0.4).max(0.0));
            }
        }
        let hits = Arc::clone(&self.hits);
        let digits = hits.last().map_or(1, |h| h.line_no.to_string().len());
        let sel = self.selection();
        let n_sel = self.selected_count();
        let mut clicked = None;
        let mut action = None;
        area.show_rows(ui, row_h, hits.len(), |ui, range| {
            self.results_visible = range.clone();
            for i in range {
                let hit = hits[i];
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let selected = sel.is_some_and(|(lo, hi)| (lo..=hi).contains(&i));
                let visuals = ui.visuals().clone();
                let painter = ui.painter_at(rect);
                if selected {
                    painter.rect_filled(rect, 3.0, visuals.selection.bg_fill);
                    if n_sel > 1 && self.selected == Some(i) {
                        let stroke = Stroke::new(1.5, visuals.selection.stroke.color);
                        painter.rect_stroke(rect, 3.0, stroke, StrokeKind::Inside);
                    }
                } else if resp.hovered() {
                    painter.rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
                }
                let text_color = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
                let num = painter.layout_no_wrap(
                    format!("{:>digits$}", fmt::count(hit.line_no), digits = digits + (digits - 1) / 3),
                    font.clone(),
                    visuals.weak_text_color(),
                );
                let num_w = num.size().x;
                painter.galley(rect.left_top() + Vec2::new(6.0, 2.0), num, text_color);

                let snippet = match &self.file {
                    Some(f) => self
                        .snippets
                        .entry(i)
                        .or_insert_with(|| {
                            lines::read_snippet(&f.file, hit.line_start, hit.match_offset(), SNIPPET_BEFORE, SNIPPET_TOTAL)
                                .unwrap_or_default()
                        })
                        .clone(),
                    None => Snippet::default(),
                };
                let job = highlight_job(
                    &snippet.bytes,
                    self.highlight.as_ref(),
                    &font,
                    text_color,
                    if snippet.cut_start { "…" } else { "" },
                    if snippet.cut_end { "…" } else { "" },
                    visuals.weak_text_color(),
                );
                let galley = painter.layout_job(job);
                painter.galley(rect.left_top() + Vec2::new(num_w + 20.0, 2.0), galley, text_color);

                if resp.clicked() {
                    clicked = Some((i, ui.input(|inp| inp.modifiers.shift)));
                }
                // Right-clicking outside the selection selects that row first.
                if resp.secondary_clicked() && !selected {
                    clicked = Some((i, false));
                }
                resp.context_menu(|ui| {
                    let n = if selected { n_sel } else { 1 };
                    let (copy, numbered) = if n == 1 {
                        ("Copy line".to_string(), "Copy line with its number".to_string())
                    } else {
                        let n = fmt::count(n as u64);
                        (format!("Copy {n} lines"), format!("Copy {n} lines with line numbers"))
                    };
                    if ui.add(egui::Button::new(copy).shortcut_text("Ctrl+C")).clicked() {
                        action = Some(ResultAction::Copy { with_line_numbers: false });
                        ui.close();
                    }
                    if ui.button(numbered).clicked() {
                        action = Some(ResultAction::Copy { with_line_numbers: true });
                        ui.close();
                    }
                    if ui.button("Copy line number").clicked() {
                        ui.ctx().copy_text(hit.line_no.to_string());
                        ui.close();
                    }
                    ui.separator();
                    if ui.add(egui::Button::new("Select all").shortcut_text("Ctrl+A")).clicked() {
                        action = Some(ResultAction::SelectAll);
                        ui.close();
                    }
                });
            }
        });
        if self.snippets.len() > 4000 {
            self.snippets.clear();
        }
        match clicked {
            Some((i, true)) => self.extend_selection(i),
            Some((i, false)) => self.select_hit(i),
            None => {}
        }
        match action {
            Some(ResultAction::Copy { with_line_numbers }) => self.copy_selected(&ctx, with_line_numbers),
            Some(ResultAction::SelectAll) => self.select_all(),
            None => {}
        }
    }

    fn viewer_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(file_len) = self.file.as_ref().map(|f| f.len) else { return };
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("File view").strong());
            if let Some(cur) = self.viewer.current {
                let at = match self.viewer_current_line_no() {
                    Some(n) => format!("line {}  ·  byte {}", fmt::count(n), fmt::count(cur)),
                    None => format!("byte {}", fmt::count(cur)),
                };
                ui.weak(at);
            }
            ui.separator();

            let goto_label = ui.label("Go to line");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.goto_text)
                    .id(egui::Id::unique("goto-line"))
                    .desired_width(110.0)
                    .hint_text("e.g. 1000000"),
            )
            .labelled_by(goto_label.id);
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if (ui.button("Go").clicked() || enter) && self.goto.is_none() {
                self.start_goto(&ctx);
            }
            if let Some(job) = &self.goto {
                let scanned = job.scanned.load(Ordering::Relaxed);
                ui.add(egui::Spinner::new());
                ui.weak(format!(
                    "counting lines… {:.0}%",
                    scanned as f64 * 100.0 / file_len.max(1) as f64
                ));
                if ui.small_button("Cancel").clicked() {
                    job.cancel.store(true, Ordering::Relaxed);
                }
            }
            ui.separator();

            if ui.small_button("⏮").on_hover_text("Start of file").clicked() {
                self.view_at(0, Some(1));
            }
            if !self.slider_active && file_len > 0 {
                self.jump_pct = (self.viewer.top_offset as f64 * 100.0 / file_len as f64) as f32;
            }
            let before = self.jump_pct;
            let slider = ui.add(
                egui::Slider::new(&mut self.jump_pct, 0.0..=100.0)
                    .suffix("%")
                    .fixed_decimals(2)
                    .trailing_fill(true)
                    .text("of file"),
            );
            self.slider_active = slider.dragged() || slider.has_focus();
            // The slider rounds the value we set (to 2 decimals) and reports
            // that as a change; ignore changes that small.
            let rounding_only = (self.jump_pct - before).abs() < 0.0075;
            if slider.drag_stopped() || (slider.changed() && !slider.dragged() && !rounding_only) {
                let offset = (self.jump_pct as f64 / 100.0 * file_len as f64) as u64;
                self.view_at(offset.min(file_len), None);
                self.slider_active = false;
            }
            if ui.small_button("⏭").on_hover_text("End of file").clicked() {
                self.view_at(file_len, None);
            }
        });
        ui.separator();

        let font = TextStyle::Monospace.resolve(ui.style());
        let row_h = ui.text_style_height(&TextStyle::Monospace) + 2.0;
        let stride = row_h + ui.spacing().item_spacing.y;
        let mut area = ScrollArea::both().id_salt("viewer").auto_shrink([false, false]);
        if let Some(row) = self.viewer.scroll_to_row.take() {
            area = area.vertical_scroll_offset((row as f32 * stride - ui.available_height() * 0.3).max(0.0));
            self.viewer.scroll_adjust = 0.0;
        } else if self.viewer.scroll_adjust != 0.0 {
            area = area.vertical_scroll_offset((self.viewer.last_offset_y + self.viewer.scroll_adjust).max(0.0));
            self.viewer.scroll_adjust = 0.0;
        }

        let n_lines = self.viewer.lines.len();
        let gutter_text = |v: &Viewer, i: usize| match v.first_line_no {
            Some(first) => fmt::count(first + i as u64),
            None => format!("@{}", v.lines[i].offset),
        };
        let gutter_chars = if n_lines == 0 { 1 } else { gutter_text(&self.viewer, n_lines - 1).len() };
        let mut copy = None;
        let mut mark = None;
        let out = area.show_rows(ui, row_h, n_lines, |ui, range| {
            let visuals = ui.visuals().clone();
            let gutter = ui.painter().layout_no_wrap("0".repeat(gutter_chars), font.clone(), Color32::GRAY).size().x + 16.0;
            for i in range.clone() {
                let line = &self.viewer.lines[i];
                let suffix = if line.is_cut() {
                    format!("  … (+{} more)", fmt::bytes(line.len - line.bytes.len() as u64))
                } else {
                    String::new()
                };
                let job = highlight_job(
                    &line.bytes,
                    self.highlight.as_ref(),
                    &font,
                    visuals.text_color(),
                    "",
                    &suffix,
                    visuals.weak_text_color(),
                );
                let galley = ui.painter().layout_job(job);
                let width = (gutter + galley.size().x + 12.0).max(ui.available_width());
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, row_h), Sense::click());
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let painter = ui.painter();
                if Some(line.offset) == self.viewer.current {
                    painter.rect_filled(rect, 0.0, visuals.selection.bg_fill.gamma_multiply(0.45));
                } else if resp.hovered() {
                    painter.rect_filled(rect, 0.0, visuals.faint_bg_color);
                }
                let num = gutter_text(&self.viewer, i);
                let num_galley = painter.layout_no_wrap(num, font.clone(), visuals.weak_text_color());
                let num_x = rect.left() + gutter - 10.0 - num_galley.size().x;
                painter.galley(egui::pos2(num_x, rect.top() + 1.0), num_galley, visuals.weak_text_color());
                painter.galley(egui::pos2(rect.left() + gutter, rect.top() + 1.0), galley, visuals.text_color());
                if resp.clicked() {
                    mark = Some(line.offset);
                }
                let line_no = self.viewer.first_line_no.map(|f| f + i as u64);
                let offset = line.offset;
                resp.context_menu(|ui| {
                    if ui.button("Copy line").clicked() {
                        copy = Some((offset, line_no));
                        ui.close();
                    }
                    if let Some(n) = line_no {
                        if ui.button("Copy line number").clicked() {
                            ui.ctx().copy_text(n.to_string());
                            ui.close();
                        }
                    }
                });
            }
            range
        });
        self.viewer.last_offset_y = out.state.clamped_offset().y;
        let range = out.inner;
        if let Some(l) = self.viewer.lines.get(range.start.min(n_lines.saturating_sub(1))) {
            self.viewer.top_offset = if range.start == 0 { self.viewer.start } else { l.offset };
        }
        if let Some(off) = mark {
            self.viewer.current = Some(off);
            self.copy_from_viewer = true;
        }
        if let Some((off, n)) = copy {
            self.copy_line(&ctx, off, n);
        }

        // Load more text when scrolling close to either edge.
        const MARGIN: usize = 40;
        if n_lines > 0 && range.start < MARGIN && self.viewer.start > 0 {
            self.viewer_load_above(stride);
        } else if range.end + MARGIN > n_lines && self.viewer.end < file_len {
            self.viewer_load_below(stride);
        }
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        if ctx.input(|i| i.raw.hovered_files.is_empty()) {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop-overlay")));
        let rect = ctx.content_rect();
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(170));
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Drop to open",
            FontId::proportional(28.0),
            Color32::WHITE,
        );
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_input(&ctx);
        self.poll_search();
        self.poll_jobs(&ctx);

        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if self.file.is_some() {
            egui::Panel::bottom("viewer")
                .resizable(true)
                .default_size(360.0)
                .min_size(140.0)
                .show(ui, |ui| self.viewer_ui(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if self.file.is_some() {
                self.results_ui(ui);
            } else {
                self.welcome(ui);
            }
        });
        self.drop_overlay(&ctx);

        if self.search.is_some() || self.goto.is_some() || self.export.is_some() || self.copy.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

// -------------------------------------------------------------------- helpers

fn load_view(f: &OpenFile, offset: u64, line_no: Option<u64>) -> std::io::Result<Viewer> {
    let offset = offset.min(f.len);
    let mut ls = lines::line_start_containing(&f.file, offset)?;
    let mut line_no = line_no;
    if ls >= f.len && f.len > 0 {
        // Past the final line break: show the last line instead.
        ls = lines::line_start_containing(&f.file, f.len - 1)?;
        line_no = None;
    }
    let before = lines::read_lines_backward(&f.file, f.len, ls, VIEW_CONTEXT_ABOVE)?;
    let (after, end) = lines::read_lines_forward(&f.file, f.len, ls, VIEW_STEP_LINES)?;
    let line_no = match line_no {
        Some(n) => Some(n),
        None => lines::line_number_at(&f.file, &f.index, ls)?,
    };
    let row = before.len();
    let start = before.first().map_or(ls, |l| l.offset);
    let mut all = before;
    all.extend(after);
    Ok(Viewer {
        lines: all,
        start,
        end: end.max(ls),
        first_line_no: line_no.map(|n| n.saturating_sub(row as u64)),
        current: Some(ls),
        scroll_to_row: Some(row),
        top_offset: ls,
        ..Viewer::default()
    })
}

/// The search box's text layout: with several search texts, each line is
/// shown in the color its matches get.
fn search_box_layout(text: &str, font: &FontId, color: Color32) -> LayoutJob {
    let normal = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let several = search_terms(text).len() > 1;
    let mut job = LayoutJob::default();
    let mut term = 0;
    for piece in text.split_inclusive('\n') {
        let body = piece.strip_suffix('\n').unwrap_or(piece);
        if several && !body.trim_end_matches('\r').is_empty() {
            let marked = TextFormat { color: Color32::BLACK, background: term_color(term), ..normal.clone() };
            job.append(body, 0.0, marked);
            term += 1;
        } else {
            job.append(body, 0.0, normal.clone());
        }
        if body.len() < piece.len() {
            job.append("\n", 0.0, normal.clone());
        }
    }
    job
}

/// Builds a text layout of `bytes` with every match highlighted, in the
/// color of the search text it belongs to.
fn highlight_job(
    bytes: &[u8],
    matcher: Option<&Matcher>,
    font: &FontId,
    color: Color32,
    prefix: &str,
    suffix: &str,
    weak: Color32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = TextFormat { font_id: font.clone(), color, ..Default::default() };
    let dim = TextFormat { font_id: font.clone(), color: weak, ..Default::default() };
    if !prefix.is_empty() {
        job.append(prefix, 0.0, dim.clone());
    }
    let mut last = 0;
    if let Some(matcher) = matcher {
        for (m, term) in matcher.highlights(bytes, 256) {
            let marked = TextFormat { color: Color32::BLACK, background: term_color(term), ..normal.clone() };
            job.append(&display_text(&bytes[last..m.start]), 0.0, normal.clone());
            job.append(&display_text(&bytes[m.start..m.end]), 0.0, marked);
            last = m.end;
        }
    }
    job.append(&display_text(&bytes[last..]), 0.0, normal);
    if !suffix.is_empty() {
        job.append(suffix, 0.0, dim);
    }
    job
}

/// Bytes -> printable text: invalid UTF-8 becomes �, tabs become spaces,
/// other control characters become a middle dot.
fn display_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

fn sniff_encoding(file: &File, len: u64) -> Option<String> {
    let mut buf = vec![0u8; (64 << 10).min(len as usize)];
    let n = read_full_at(file, &mut buf, 0).ok()?;
    let head = &buf[..n];
    if head.starts_with(&[0xFF, 0xFE]) || head.starts_with(&[0xFE, 0xFF]) {
        return Some(
            "This file is UTF-16 encoded. Searching works on UTF-8/ASCII text, so most searches will find \
             nothing. Re-save it as UTF-8 first."
                .into(),
        );
    }
    let zeros = head.iter().filter(|&&b| b == 0).count();
    (n > 0 && zeros * 20 > n).then(|| {
        "This file contains many zero bytes. It may be binary or UTF-16 without a marker, so text may look garbled."
            .into()
    })
}

/// Adds Windows system fonts as fallbacks so CJK, Korean and symbol
/// characters render instead of showing boxes.
fn install_fallback_fonts(ctx: &egui::Context) {
    let candidates = [
        ("msyh", r"C:\Windows\Fonts\msyh.ttc"),
        ("malgun", r"C:\Windows\Fonts\malgun.ttf"),
        ("seguisym", r"C:\Windows\Fonts\seguisym.ttf"),
    ];
    for (name, path) in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let families = [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                .into_iter()
                .map(|family| egui::epaint::text::InsertFontFamily {
                    family,
                    priority: egui::epaint::text::FontPriority::Lowest,
                })
                .collect();
            ctx.add_font(egui::epaint::text::FontInsert::new(name, egui::FontData::from_owned(bytes), families));
        }
    }
}

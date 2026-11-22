//! Headless UI tests: drive the real app with simulated input through
//! egui_kittest, without opening a window.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Key, Modifiers};
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

use super::App;

const LINES: u64 = 200_000;

struct TempFile(PathBuf);

impl TempFile {
    /// "line N <padding>" for N in 1..=LINES, with "needle" on two lines.
    fn sample() -> Self {
        let path = std::env::temp_dir().join(format!("hfr-ui-{}-{:?}.txt", std::process::id(), std::thread::current().id()));
        let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        for n in 1..=LINES {
            writeln!(f, "{}", sample_line(n)).unwrap();
        }
        f.flush().unwrap();
        TempFile(path)
    }
}

/// Line `n` of the sample file, without its line break.
fn sample_line(n: u64) -> String {
    let extra = if n == 50_000 || n == 150_000 { " has a needle here" } else { "" };
    format!("line {n} {}{extra}", "x".repeat((n % 40) as usize))
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn harness(file: &TempFile, query: &str) -> Harness<'static, App> {
    let (path, query) = (file.0.clone(), query.to_string());
    Harness::builder()
        .with_size([1280.0, 860.0])
        .build_eframe(move |cc| App::new(cc, Some(path), Some(query)))
}

/// Steps frames until `done` holds (background threads need real time).
fn run_until(h: &mut Harness<'static, App>, what: &str, done: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done(h.state()) {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    h.run_steps(3);
}

/// The number shown in the viewer for the marked line, and that line's text.
fn marked_line(app: &App) -> (u64, String) {
    let v = &app.viewer;
    let cur = v.current.expect("a marked line");
    let idx = v.lines.iter().position(|l| l.offset == cur).expect("marked line is loaded");
    let n = v.first_line_no.expect("line numbers known") + idx as u64;
    (n, String::from_utf8_lossy(&v.lines[idx].bytes).into_owned())
}

/// Every loaded viewer line must carry the number printed inside it.
fn assert_viewer_numbering(app: &App) {
    let v = &app.viewer;
    let first = v.first_line_no.expect("line numbers known");
    assert!(!v.lines.is_empty());
    for (i, l) in v.lines.iter().enumerate() {
        let text = String::from_utf8_lossy(&l.bytes);
        let want = format!("line {} ", first + i as u64);
        assert!(text.starts_with(&want), "row {i}: expected {want:?}, got {text:?}");
    }
    // Rows are contiguous in the file.
    for w in v.lines.windows(2) {
        assert_eq!(w[0].offset + w[0].len + 1, w[1].offset);
    }
    assert_eq!(v.start, v.lines[0].offset);
}

#[test]
fn search_select_navigate_and_goto() {
    let file = TempFile::sample();
    let mut h = harness(&file, "NEEDLE");
    run_until(&mut h, "search to finish", |a| a.search.is_none() && a.summary.is_some());

    let app = h.state();
    let s = app.summary.as_ref().unwrap();
    assert_eq!(s.matched_lines, 2);
    assert_eq!(s.total_lines, Some(LINES));
    assert_eq!(app.hits.iter().map(|h| h.line_no).collect::<Vec<_>>(), [50_000, 150_000]);
    assert_eq!(app.file.as_ref().unwrap().total_lines, Some(LINES));

    // The first hit is selected and shown in the viewer, with correct numbering.
    assert_eq!(app.selected, Some(0));
    let (n, text) = marked_line(app);
    assert_eq!(n, 50_000);
    assert!(text.contains("needle"), "{text}");
    assert_viewer_numbering(app);

    // F3 moves to the next hit, Shift+F3 back.
    h.key_press(Key::F3);
    h.run_steps(3);
    assert_eq!(h.state().selected, Some(1));
    assert_eq!(marked_line(h.state()).0, 150_000);
    h.key_press_modifiers(Modifiers::SHIFT, Key::F3);
    h.run_steps(3);
    assert_eq!(h.state().selected, Some(0));
    assert_eq!(marked_line(h.state()).0, 50_000);

    // Go to line via the text box and the Go button.
    h.get_by_label("Go to line").click();
    h.run_steps(2);
    h.get_by_label("Go to line").type_text("123,456");
    h.run_steps(2);
    h.get_by_label("Go").click();
    run_until(&mut h, "go to line", |a| a.goto.is_none() && marked_line(a).0 == 123_456);
    let (n, text) = marked_line(h.state());
    assert_eq!(n, 123_456);
    assert!(text.starts_with("line 123456 "), "{text}");
    assert_viewer_numbering(h.state());

    // Going past the end reports an error instead of moving.
    h.state_mut().goto_text = "999999999".into();
    h.get_by_label("Go").click();
    run_until(&mut h, "failed go to line", |a| a.goto.is_none() && a.status.is_some());
    assert!(h.state().status.as_ref().unwrap().text.contains("200,000 lines"));
    assert_eq!(marked_line(h.state()).0, 123_456);
}

#[test]
fn viewer_scrolls_and_loads_more_text() {
    let file = TempFile::sample();
    let mut h = harness(&file, "needle");
    run_until(&mut h, "search to finish", |a| a.search.is_none() && a.summary.is_some());
    // Jump into the middle so there's room in both directions.
    h.state_mut().goto_text = "100000".into();
    h.get_by_label("Go").click();
    run_until(&mut h, "go to line", |a| a.goto.is_none() && marked_line(a).0 == 100_000);
    let start_before = h.state().viewer.start;

    // Hover the viewer and scroll up a lot: earlier lines must get loaded.
    let viewer_pos = egui::pos2(600.0, 700.0);
    h.event(egui::Event::PointerMoved(viewer_pos));
    for _ in 0..40 {
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Page,
            delta: egui::vec2(0.0, 1.0),
            modifiers: Modifiers::NONE,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
        });
        h.run_steps(2);
    }
    let v = &h.state().viewer;
    assert!(v.start < start_before, "scrolling up should load earlier lines");
    assert!(v.lines.len() <= super::VIEW_MAX_LINES);
    assert_viewer_numbering(h.state());

    // And scroll down far past the original window.
    let end_before = h.state().viewer.end;
    for _ in 0..120 {
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Page,
            delta: egui::vec2(0.0, -1.0),
            modifiers: Modifiers::NONE,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Wheel,
        });
        h.run_steps(2);
    }
    let v = &h.state().viewer;
    assert!(v.end > end_before, "scrolling down should load later lines");
    assert!(v.lines.len() <= super::VIEW_MAX_LINES);
    assert_viewer_numbering(h.state());
}

#[test]
fn end_of_file_and_bad_regex() {
    let file = TempFile::sample();
    let mut h = harness(&file, "(unclosed");
    h.state_mut().use_regex = true;
    // The pending query already ran as plain text; run it again as a regex.
    run_until(&mut h, "first search", |a| a.search.is_none() && a.summary.is_some());
    h.get_by_label("🔍  Search").click();
    h.run_steps(3);
    let st = h.state().status.as_ref().expect("an error message");
    assert!(st.error && st.text.contains("Invalid regular expression"), "{}", st.text);

    // The "end of file" button shows the last line, numbered once known.
    h.get_by_label("⏭").click();
    h.run_steps(3);
    let (n, text) = marked_line(h.state());
    assert_eq!(n, LINES);
    assert!(text.starts_with(&format!("line {LINES} ")), "{text}");
}

/// Steps frames until the app puts text on the clipboard, and returns it.
fn wait_for_copy(h: &mut Harness<'static, App>) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(Instant::now() < deadline, "timed out waiting for a copy");
        h.step();
        for c in &h.output().platform_output.commands {
            if let egui::OutputCommand::CopyText(t) = c {
                return t.clone();
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn hit_lines(app: &App) -> Vec<u64> {
    app.hits.iter().map(|h| h.line_no).collect()
}

fn search_box_focused(h: &Harness<'static, App>) -> bool {
    h.ctx.memory(|m| m.has_focus(super::search_box_id()))
}

#[test]
fn several_search_texts_any_or_all() {
    let file = TempFile::sample();
    // One search text per line: lines with any of them.
    let mut h = harness(&file, "needle\nline 777 ");
    run_until(&mut h, "search to finish", |a| a.search.is_none() && a.summary.is_some());
    assert_eq!(hit_lines(h.state()), [777, 50_000, 150_000]);
    let m = h.state().highlight.as_ref().unwrap();
    assert_eq!(m.terms(), 2);
    // Each text is highlighted in its own color.
    assert_eq!(m.highlights(sample_line(50_000).as_bytes(), 10)[0].1, 0);
    assert_eq!(m.highlights(sample_line(777).as_bytes(), 10)[0].1, 1);

    // Lines with all of them.
    h.state_mut().query = "needle\nline 150000 ".into();
    h.run_steps(2);
    h.get_by_label("all of them").click();
    h.run_steps(2);
    assert!(h.state().match_all);
    h.get_by_label("🔍  Search").click();
    h.run_steps(3);
    run_until(&mut h, "second search", |a| a.search.is_none() && a.summary.is_some());
    assert_eq!(hit_lines(h.state()), [150_000]);

    // Empty lines in the box are ignored; one text left means a plain search.
    h.state_mut().query = "\n\nline 4242 \n".into();
    h.get_by_label("🔍  Search").click();
    h.run_steps(3);
    run_until(&mut h, "third search", |a| a.search.is_none() && a.summary.is_some());
    assert_eq!(hit_lines(h.state()), [4242]);
}

#[test]
fn select_all_and_copy_found_lines() {
    let file = TempFile::sample();
    let mut h = harness(&file, "needle\nline 777 ");
    run_until(&mut h, "search to finish", |a| a.search.is_none() && a.summary.is_some());
    assert!(!search_box_focused(&h), "the results list should have the keyboard");

    // Ctrl+A selects every found line, Ctrl+C copies them.
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert_eq!(h.state().selected_count(), 3);
    h.event(egui::Event::Copy);
    assert_eq!(wait_for_copy(&mut h), [777, 50_000, 150_000].map(sample_line).join("\r\n"));
    run_until(&mut h, "copy to finish", |a| a.copy.is_none());
    assert_eq!(h.state().status.as_ref().unwrap().text, "3 lines copied to the clipboard.");

    // Home selects one line again; Shift+Down extends the selection.
    h.key_press(Key::Home);
    h.run_steps(2);
    assert_eq!(h.state().selection(), Some((0, 0)));
    h.key_press_modifiers(Modifiers::SHIFT, Key::ArrowDown);
    h.run_steps(2);
    assert_eq!(h.state().selection(), Some((0, 1)));
    assert_eq!(h.state().selected, Some(1));
    h.event(egui::Event::Copy);
    assert_eq!(wait_for_copy(&mut h), [777, 50_000].map(sample_line).join("\r\n"));
    run_until(&mut h, "copy to finish", |a| a.copy.is_none());

    // Clicking a line in the file view makes Ctrl+C copy that line.
    let pos = egui::pos2(600.0, 700.0);
    h.event(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE });
        h.run_steps(1);
    }
    h.run_steps(2);
    assert!(h.state().copy_from_viewer);
    let (n, _) = marked_line(h.state());
    h.event(egui::Event::Copy);
    assert_eq!(wait_for_copy(&mut h), sample_line(n));

    // Typing while the list has the keyboard goes into the search box.
    h.event(egui::Event::Text("abc".into()));
    h.run_steps(2);
    assert!(search_box_focused(&h));
    assert_eq!(h.state().query, "needle\nline 777 abc");
    // Shift+Enter starts another search text, Enter searches.
    h.key_press_modifiers(Modifiers::SHIFT, Key::Enter);
    h.run_steps(2);
    h.event(egui::Event::Text("line 123 ".into()));
    h.run_steps(2);
    assert_eq!(h.state().query, "needle\nline 777 abc\nline 123 ");
    assert!(h.state().search.is_none(), "Shift+Enter must not search");
    h.key_press(Key::Enter);
    h.run_steps(3);
    run_until(&mut h, "search to finish", |a| a.search.is_none() && a.summary.is_some());
    assert_eq!(hit_lines(h.state()), [123, 50_000, 150_000]);
    assert!(!search_box_focused(&h));

    // While the search box has the keyboard, Ctrl+A selects its text, not the results.
    h.state_mut().focus_search = true;
    h.run_steps(2);
    assert!(search_box_focused(&h));
    h.key_press_modifiers(Modifiers::COMMAND, Key::A);
    h.run_steps(2);
    assert_eq!(h.state().selected_count(), 1);
}

//! A local link for other apps that want to follow the selected results. The
//! other app listens on a local port (127.0.0.1 only); a background thread
//! here connects whenever it's running, sends it the selection, and passes on
//! the results it asks to show.
//!
//! The protocol is UTF-8 lines with tab-separated fields. Inside a field,
//! `\`, tab, CR and LF are written as `\\`, `\t`, `\r` and `\n`.
//!
//! Huge File Reader → the other app:
//! - `HELLO <version>`
//! - `SEL <selected lines> <current result or -> <file path>` starts a
//!   selection, followed by:
//!   - `FIND <regex 0|1> <match case 0|1> <whole word 0|1>`
//!   - `TERM <search text>`, one per search text
//!   - `HIT <result> <line number> <line start> <match offset>`, one per listed line
//!   - `END`
//! - `CUR <current result or ->`: only the current result changed
//! - `CLEAR`: nothing is selected
//! - `PING`: nothing changed
//!
//! The other app → Huge File Reader:
//! - `SHOW <result> <line start>`: make that result the current one

use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};

/// The port both apps use unless `HFR_LINK_PORT` says otherwise.
pub const DEFAULT_PORT: u16 = 47391;

pub fn port() -> u16 {
    std::env::var("HFR_LINK_PORT").ok().and_then(|p| p.trim().parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// The search that found the results, for highlighting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    pub terms: Vec<String>,
    pub regex: bool,
    pub match_case: bool,
    pub whole_word: bool,
}

/// One selected result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Its position in the results list.
    pub index: usize,
    pub line_no: u64,
    pub line_start: u64,
    pub match_offset: u64,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub path: PathBuf,
    /// How many results are selected; `hits` may list fewer.
    pub total: u64,
    pub current: Option<usize>,
    pub search: Search,
    /// Shared between selections that list the same results, so a change
    /// of just the current result is sent as such.
    pub hits: Arc<Vec<Hit>>,
}

impl Selection {
    fn same_list(&self, other: &Selection) -> bool {
        Arc::ptr_eq(&self.hits, &other.hits)
            && self.path == other.path
            && self.total == other.total
            && self.search == other.search
    }
}

/// The other app asks to make result `index` (which starts at `line_start`) the
/// current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Show {
    pub index: usize,
    pub line_start: u64,
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

pub struct Link {
    shared: Arc<Shared>,
    pub shows: Receiver<Show>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    connected: AtomicBool,
    stop: AtomicBool,
}

#[derive(Default)]
struct State {
    version: u64,
    latest: Option<Arc<Selection>>,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

impl Link {
    /// Starts connecting to the app listening on `port`, again and again
    /// while none is. `waker` is called when something arrives or the
    /// connection comes or goes.
    pub fn start(port: u16, waker: impl Fn() + Send + Sync + 'static) -> Link {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            connected: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let (tx, shows) = unbounded();
        let s = Arc::clone(&shared);
        let waker: Waker = Arc::new(waker);
        std::thread::spawn(move || run(&s, port, &tx, &waker));
        Link { shared, shows }
    }

    /// Makes `selection` what the other app is sent (`None`: nothing selected).
    pub fn publish(&self, selection: Option<Arc<Selection>>) {
        let mut st = self.shared.state.lock().unwrap();
        st.version += 1;
        st.latest = selection;
        self.shared.changed.notify_all();
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Relaxed)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _guard = self.shared.state.lock().unwrap();
        self.shared.changed.notify_all();
    }
}

fn run(shared: &Arc<Shared>, port: u16, tx: &Sender<Show>, waker: &Waker) {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    while !shared.stop.load(Ordering::Relaxed) {
        if let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            shared.connected.store(true, Ordering::Relaxed);
            waker();
            serve(shared, stream, tx, waker);
            shared.connected.store(false, Ordering::Relaxed);
            waker();
        }
        let st = shared.state.lock().unwrap();
        if !shared.stop.load(Ordering::Relaxed) {
            let _ = shared.changed.wait_timeout(st, Duration::from_secs(1));
        }
    }
}

/// Keeps the other app up to date until the connection or the app closes.
fn serve(shared: &Arc<Shared>, stream: TcpStream, tx: &Sender<Show>, waker: &Waker) {
    let _ = stream.set_nodelay(true);
    let alive = Arc::new(AtomicBool::new(true));
    if let Ok(read) = stream.try_clone() {
        let (alive, shared, tx, waker) = (Arc::clone(&alive), Arc::clone(shared), tx.clone(), Arc::clone(waker));
        std::thread::spawn(move || {
            let mut reader = BufReader::new(read);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Some(show) = parse_show(&String::from_utf8_lossy(&line)) {
                            let _ = tx.send(show);
                            waker();
                        }
                    }
                }
            }
            alive.store(false, Ordering::Relaxed);
            let _guard = shared.state.lock().unwrap();
            shared.changed.notify_all();
        });
    }
    let mut out = BufWriter::new(&stream);
    let mut sent_version = None;
    let mut sent: Option<Arc<Selection>> = None;
    let mut result = writeln!(out, "HELLO\t1");
    while result.is_ok() {
        let (version, latest) = {
            let mut st = shared.state.lock().unwrap();
            let idle = Some(st.version) == sent_version;
            if idle && alive.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
                st = shared.changed.wait_timeout(st, Duration::from_secs(2)).unwrap().0;
            }
            (st.version, st.latest.clone())
        };
        if shared.stop.load(Ordering::Relaxed) || !alive.load(Ordering::Relaxed) {
            break;
        }
        result = if Some(version) == sent_version {
            writeln!(out, "PING")
        } else {
            write_update(&mut out, sent.as_deref(), latest.as_deref())
        }
        .and_then(|_| out.flush());
        sent_version = Some(version);
        sent = latest;
    }
    drop(out);
    let _ = stream.shutdown(Shutdown::Both);
}

/// What the other app needs to go from showing `old` to showing `new`.
fn write_update(out: &mut impl Write, old: Option<&Selection>, new: Option<&Selection>) -> io::Result<()> {
    let Some(new) = new else { return writeln!(out, "CLEAR") };
    let current = new.current.map_or("-".to_string(), |i| i.to_string());
    if old.is_some_and(|o| o.same_list(new)) {
        return writeln!(out, "CUR\t{current}");
    }
    writeln!(out, "SEL\t{}\t{current}\t{}", new.total, escape(&new.path.to_string_lossy()))?;
    let s = &new.search;
    writeln!(out, "FIND\t{}\t{}\t{}", s.regex as u8, s.match_case as u8, s.whole_word as u8)?;
    for t in &s.terms {
        writeln!(out, "TERM\t{}", escape(t))?;
    }
    for h in new.hits.iter() {
        writeln!(out, "HIT\t{}\t{}\t{}\t{}", h.index, h.line_no, h.line_start, h.match_offset)?;
    }
    writeln!(out, "END")
}

fn parse_show(line: &str) -> Option<Show> {
    let mut f = line.trim_end_matches(['\r', '\n']).split('\t');
    (f.next()? == "SHOW").then_some(())?;
    Some(Show { index: f.next()?.parse().ok()?, line_start: f.next()?.parse().ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn selection(current: Option<usize>, hits: &Arc<Vec<Hit>>) -> Option<Arc<Selection>> {
        Some(Arc::new(Selection {
            path: PathBuf::from("C:\\logs\\big\tfile.log"),
            total: 2,
            current,
            search: Search { terms: vec!["Smith".into()], match_case: true, ..Search::default() },
            hits: Arc::clone(hits),
        }))
    }

    #[test]
    fn sends_selections_and_receives_shows() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let link = Link::start(listener.local_addr().unwrap().port(), || {});
        let hits = Arc::new(vec![
            Hit { index: 3, line_no: 10, line_start: 500, match_offset: 510 },
            Hit { index: 4, line_no: 12, line_start: 700, match_offset: 702 },
        ]);
        link.publish(selection(Some(3), &hits));
        let (stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut next = || {
            let mut l = String::new();
            reader.read_line(&mut l).unwrap();
            l
        };
        let got: Vec<String> = (0..7).map(|_| next()).collect();
        assert_eq!(
            got.concat(),
            "HELLO\t1\nSEL\t2\t3\tC:\\\\logs\\\\big\\tfile.log\nFIND\t0\t1\t0\nTERM\tSmith\n\
             HIT\t3\t10\t500\t510\nHIT\t4\t12\t700\t702\nEND\n"
        );
        assert!(link.is_connected());

        // Only the current result changes.
        link.publish(selection(Some(4), &hits));
        assert_eq!(next(), "CUR\t4\n");
        // The other app asks to show a result.
        (&stream).write_all(b"SHOW\t4\t700\r\n").unwrap();
        assert_eq!(link.shows.recv_timeout(Duration::from_secs(5)).unwrap(), Show { index: 4, line_start: 700 });
        link.publish(None);
        assert_eq!(next(), "CLEAR\n");

        // The other app closes; the link notices.
        drop(listener);
        drop(reader);
        stream.shutdown(Shutdown::Both).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while link.is_connected() {
            assert!(std::time::Instant::now() < deadline, "the link should notice the other app closed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

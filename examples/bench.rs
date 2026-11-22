//! Headless search benchmark.
//!
//! cargo run --release --example bench -- <file> <text> [<text>...] [--all] [--regex] [--io=N] [--threads=N]

use std::path::Path;
use std::time::Instant;

use hfr::fileio::detect_drive_kind;
use hfr::fmt;
use hfr::search::{search_blocking, SearchOptions};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if positional.len() < 2 {
        eprintln!("usage: bench <file> <text> [<text>...] [--all] [--regex] [--io=N] [--threads=N]");
        std::process::exit(2);
    }
    let flag = |name: &str| args.iter().find_map(|a| a.strip_prefix(&format!("--{name}="))?.parse::<usize>().ok());
    let path = Path::new(positional[0]);
    let drive = detect_drive_kind(path);
    eprintln!("drive: {}", drive.label());
    let io = flag("io").unwrap_or(drive.io_parallelism());
    let threads = flag("threads").unwrap_or(0);
    let opts = SearchOptions {
        regex: args.iter().any(|a| a == "--regex"),
        match_all: args.iter().any(|a| a == "--all"),
        io_parallelism: io,
        threads,
        ..SearchOptions::any_of(positional[1..].iter().map(|s| s.as_str()))
    };
    let t = Instant::now();
    let (hits, s) = search_blocking(path, opts).unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!(
        "drive={} io={io} threads={threads} | {} in {:.2}s = {}/s | {} matching lines, first at line {:?}, total lines {:?}",
        drive.label(),
        fmt::bytes(s.bytes_searched),
        secs,
        fmt::bytes((s.bytes_searched as f64 / secs) as u64),
        fmt::count(s.matched_lines),
        hits.first().map(|h| h.line_no),
        s.total_lines.map(fmt::count),
    );
}

//! Generates a large log-like text file for testing.
//!
//! cargo run --release --example gen -- <output path> <size in GB>

use std::fs::File;
use std::io::{BufWriter, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: gen <output path> <size in GB>");
        std::process::exit(2);
    }
    let target = (args[2].parse::<f64>().expect("size in GB") * (1u64 << 30) as f64) as u64;

    // Build one 256 MB block of varied lines, then write it repeatedly,
    // patching a block number into the first line so blocks differ.
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let levels = ["INFO", "INFO", "INFO", "DEBUG", "WARN", "INFO", "DEBUG", "INFO"];
    let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet"];
    let mut block = Vec::with_capacity(256 << 20);
    block.extend_from_slice(b"BLOCK 000000 begins here\n");
    let mut n = 0u64;
    while block.len() < (256 << 20) {
        n += 1;
        let r = rnd();
        let level = if n.is_multiple_of(1_000_003) { "ERROR" } else { levels[(r % 8) as usize] };
        let line = format!(
            "2026-11-{:02} {:02}:{:02}:{:02}.{:03} {level} [worker-{}] request id={:016x} user=user{} path=/api/v1/{}/{} status={} duration_ms={}{}\n",
            1 + r % 28,
            (r >> 8) % 24,
            (r >> 16) % 60,
            (r >> 24) % 60,
            (r >> 32) % 1000,
            (r >> 40) % 32,
            rnd(),
            (r >> 12) % 100_000,
            words[((r >> 20) % 10) as usize],
            (r >> 28) % 10_000,
            [200, 200, 200, 201, 304, 404, 500][((r >> 36) % 7) as usize],
            (r >> 44) % 5000,
            if level == "ERROR" { " msg=\"needle in a haystack\"" } else { "" },
        );
        block.extend_from_slice(line.as_bytes());
    }

    let mut out = BufWriter::with_capacity(16 << 20, File::create(&args[1]).expect("create output"));
    let mut written = 0u64;
    let mut k = 0u64;
    while written < target {
        k += 1;
        block[6..12].copy_from_slice(format!("{:06}", k % 1_000_000).as_bytes());
        let take = ((target - written) as usize).min(block.len());
        // Cut at a line break so the file ends cleanly.
        let take = if take < block.len() {
            block[..take].iter().rposition(|&b| b == b'\n').map_or(take, |p| p + 1)
        } else {
            take
        };
        out.write_all(&block[..take]).expect("write");
        written += take as u64;
        if take < block.len() {
            break;
        }
    }
    out.flush().unwrap();
    println!("wrote {written} bytes in {k} blocks");
}

# Huge File Reader

A fast Windows app for searching and browsing text files of any size, including 100 GB and larger.
The file is never loaded into memory. It is streamed from disk while searching, and only the part
you're looking at is read for display. Opening a file is instant no matter how big it is.

## Using it

Run `target\release\huge-file-reader.exe`.

1. **Open a file** with the *Open file…* button or **Ctrl+O**, or by dropping it on the window. You can also
   pass it on the command line: `huge-file-reader.exe "D:\logs\huge.log" "text to find" "another text"`.
2. **Type what you're looking for** and press **Enter**. Matches appear while the search runs, with a
   progress bar, speed and time left. Press **Esc** to stop early.
   - **Several texts at once**: put each on its own line in the search box (**Shift+Enter** or the ➕ button
     adds a line; pasting a list works too). Then choose whether lines must contain **any of them** or
     **all of them**. Each text gets its own highlight color.
   - **Aa Match case**: upper/lower case must match (off by default).
   - **Whole word**: `cat` won't match `concatenate`.
   - **.\* Regex**: use a regular expression, e.g. `error|fail`, `^2026-11-2\d`, `status=5\d\d`.
3. **Click a result** (or use **F3** / **Shift+F3**, or **↑ ↓** in the list) to see that spot in the file view below,
   with line numbers and every match highlighted. Scroll the file view freely in both directions.
4. **Copy found lines**: **Ctrl+A** selects every found line and **Ctrl+C** copies them. **Shift+click** or
   **Shift+↑ ↓** selects a range. Right-click for *copy with line numbers* and *copy line number*.
   After you press Enter the results list has the keyboard, so these keys work right away. Just start
   typing to go back to the search box. The clipboard takes up to 64 MB; for more, use *Save matching lines…*.
   Clicking a line in the file view and pressing Ctrl+C copies that line.
5. **Go to line** jumps straight to any line number (**Ctrl+G**). The **position slider** jumps to any point in
   the file, and the ⏮ ⏭ buttons go to the start and end.
6. **Save matching lines…** writes every matching line to a new text file, optionally with line numbers.

## How fast is it?

Searching runs at the speed of your disk. Measured on this PC with a 20 GB log file:

| Search | Time | Speed |
| --- | --- | --- |
| Plain text (`needle`) | 7.3 s | 2.8 GB/s |
| Text matching 22 million lines | 7.2 s | 2.8 GB/s |
| Regular expression | 7.0 s | 2.9 GB/s |

Searching for several texts at once is just as fast: from memory the engine handles 5–9 GB/s with two or three
texts (any or all of them, plain or regex), more than the disk can deliver.

So a **100 GB file takes about 40 seconds on the NVMe SSD (C:)**. On a typical hard drive (about 100–150 MB/s)
expect **12–18 minutes**, because the disk can't deliver data any faster. The app detects SSDs and reads them in
parallel; hard drives and unknown drives are read strictly front to back, which is fastest for spinning disks.

Every search also counts lines, so after one complete search **Go to line** is instant anywhere in the file.
Before that, it counts lines from the start of the file (or from as far as an earlier search got).

## Good to know

- Works with UTF-8 and plain ASCII text, which covers almost all logs, CSVs and dumps. If a file is UTF-16 the app warns
  you, since searches won't find anything. Re-save it as UTF-8 first.
- Matching is per line, like `grep`: each matching line is listed once, at its first match.
- The list holds the first 5,000,000 matching lines. Beyond that, matches are still counted but not listed.
- Lines are shown up to 4 KB wide in the viewer. *Copy line* copies up to 16 MB of a line.

## Building from source

Requires [Rust](https://rustup.rs). From this folder:

```
cargo build --release      # builds target\release\huge-file-reader.exe
cargo test --release       # engine, line-handling and UI tests
```

Developer extras:

```
cargo run --release --example gen -- C:\temp\test.log 20        # make a 20 GB test file
cargo run --release --example bench -- C:\temp\test.log needle  # time a search without the UI
cargo run --release --example bench -- C:\temp\test.log ERROR timeout --all   # several texts, all on one line
```

### Layout

| File | What it does |
| --- | --- |
| `src/search.rs` | Parallel streaming search: chunking, line ownership across chunk edges, ordered I/O, line numbering, any/all of several texts |
| `src/lines.rs` | Reading lines around any offset, match snippets, the sparse line index and *go to line* |
| `src/fileio.rs` | Positional reads and SSD/hard-drive detection |
| `src/export.rs` | Saving matching lines to a file, or collecting them for the clipboard |
| `src/app.rs` | The user interface (egui) |
| `tests/search.rs` | Checks the engine against a simple line-by-line reference across many chunk sizes and thread counts |
| `src/app_tests.rs` | Drives the real UI headlessly: search, several texts, select all and copy, navigation, go to line, scrolling |

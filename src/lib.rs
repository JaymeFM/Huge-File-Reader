//! Core engine for Huge File Reader: streaming, multi-threaded search and
//! random-access reading of text files far larger than RAM.
//!
//! Nothing in here ever loads a whole file into memory. Search reads the file
//! in fixed-size chunks that are scanned in parallel; the viewer reads small
//! windows around a byte offset on demand.

pub mod export;
pub mod fileio;
pub mod fmt;
pub mod lines;
pub mod link;
pub mod search;

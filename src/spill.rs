//! Disk spill: an append-only temp file holding every line ever received,
//! with a sparse offset index so any line can be fetched by absolute number.
//!
//! The ring buffer stays the hot cache for the tail; the spill is the source
//! of truth for full-history scrollback and filtering. RAM cost is one `u64`
//! per [`STRIDE`] lines — everything else lives in the file, and the OS page
//! cache makes reads of recently written regions cheap.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write};

use tempfile::NamedTempFile;

/// One byte offset is indexed every `STRIDE` lines; fetching a line reads at
/// most `STRIDE - 1` lines past the indexed seek point.
const STRIDE: u64 = 64;

pub struct Spill {
    /// Keeps the temp file alive; it is removed when the `Spill` is dropped.
    _guard: NamedTempFile,
    writer: RefCell<Writer>,
    reader: RefCell<BufReader<File>>,
    /// Byte offset of every `STRIDE`-th line (lines 0, 64, 128, …).
    index: Vec<u64>,
    total: u64,
    error: Option<String>,
}

struct Writer {
    out: BufWriter<File>,
    /// File length including buffered-but-unflushed bytes.
    pos: u64,
    dirty: bool,
}

impl Spill {
    pub fn new() -> io::Result<Self> {
        let guard = NamedTempFile::new()?;
        let write_handle = guard.as_file().try_clone()?;
        let read_handle = guard.reopen()?; // independent cursor
        Ok(Self {
            _guard: guard,
            writer: RefCell::new(Writer {
                out: BufWriter::with_capacity(128 * 1024, write_handle),
                pos: 0,
                dirty: false,
            }),
            reader: RefCell::new(BufReader::new(read_handle)),
            index: Vec::new(),
            total: 0,
            error: None,
        })
    }

    /// Set once a write has failed: the spill stops growing, so history older
    /// than the ring buffer is truncated at that point.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Append one line (must not contain `\n` — lines are newline-delimited).
    /// On write failure the error is remembered and the spill stops growing;
    /// the TUI keeps running on the ring buffer alone.
    pub fn append(&mut self, line: &str) {
        if self.error.is_some() {
            return;
        }
        let w = self.writer.get_mut();
        if self.total.is_multiple_of(STRIDE) {
            self.index.push(w.pos);
        }
        let res = w
            .out
            .write_all(line.as_bytes())
            .and_then(|()| w.out.write_all(b"\n"));
        if let Err(e) = res {
            self.error = Some(e.to_string());
            return;
        }
        w.pos += line.len() as u64 + 1;
        w.dirty = true;
        self.total += 1;
    }

    /// Make everything appended so far visible to the read handle.
    fn flush(&self) -> io::Result<()> {
        let mut w = self.writer.borrow_mut();
        if w.dirty {
            w.out.flush()?;
            w.dirty = false;
        }
        Ok(())
    }

    /// Fetch one line by absolute number.
    pub fn read_line(&self, abs: u64) -> Option<String> {
        if abs >= self.total {
            return None;
        }
        self.flush().ok()?;
        let mut r = self.reader.borrow_mut();
        let start = self.index[(abs / STRIDE) as usize];
        r.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = Vec::new();
        for _ in 0..=(abs % STRIDE) {
            buf.clear();
            r.read_until(b'\n', &mut buf).ok()?;
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        Some(String::from_utf8_lossy(&buf).into_owned())
    }

    /// Stream every stored line in order (used to rebuild the filter view).
    /// The callback must not touch this `Spill` (the reader stays borrowed).
    pub fn scan(&self, mut f: impl FnMut(u64, &str)) {
        if self.total == 0 || self.flush().is_err() {
            return;
        }
        let mut r = self.reader.borrow_mut();
        if r.seek(SeekFrom::Start(0)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        for abs in 0..self.total {
            buf.clear();
            if r.read_until(b'\n', &mut buf).is_err() {
                return;
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            f(abs, &String::from_utf8_lossy(&buf));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_across_stride_boundaries() {
        let mut sp = Spill::new().unwrap();
        for i in 0..200 {
            sp.append(&format!("line-{i}"));
        }
        for abs in [0, 1, 63, 64, 65, 127, 128, 199] {
            assert_eq!(sp.read_line(abs).as_deref(), Some(format!("line-{abs}").as_str()));
        }
        assert_eq!(sp.read_line(200), None);
    }

    #[test]
    fn scan_streams_all_lines_and_interleaves_with_appends() {
        let mut sp = Spill::new().unwrap();
        sp.append("a");
        sp.append("b");
        let mut got = Vec::new();
        sp.scan(|abs, line| got.push((abs, line.to_string())));
        assert_eq!(got, vec![(0, "a".into()), (1, "b".into())]);
        sp.append("c");
        got.clear();
        sp.scan(|abs, line| got.push((abs, line.to_string())));
        assert_eq!(got.len(), 3);
        assert_eq!(got[2], (2, "c".to_string()));
        // reads still work after a scan repositioned the reader
        assert_eq!(sp.read_line(0).as_deref(), Some("a"));
    }

    #[test]
    fn empty_and_unicode_lines() {
        let mut sp = Spill::new().unwrap();
        sp.append("");
        sp.append("grüße — ünïcode");
        assert_eq!(sp.read_line(0).as_deref(), Some(""));
        assert_eq!(sp.read_line(1).as_deref(), Some("grüße — ünïcode"));
    }
}

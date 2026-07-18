//! Stdin reader thread: turns blocking line reads into channel messages so
//! the UI never blocks on I/O. A bounded channel applies backpressure to the
//! producer; the ring buffer in the UI caps total memory.

use std::io::{self, BufRead};
use std::thread;

use crossbeam_channel::Sender;

pub enum Msg {
    /// One line of input (terminators stripped).
    Line(String),
    /// stdin reached EOF: the stream is now "static".
    Eof,
}

/// Spawn the reader thread. It exits after sending `Eof`, or immediately if
/// the UI hung up (channel closed on quit).
pub fn spawn(tx: Sender<Msg>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("logtui-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            let mut line = String::new();
            loop {
                line.clear();
                match lock.read_line(&mut line) {
                    Ok(0) => break,            // EOF
                    Ok(_) => {
                        while line.ends_with('\n') || line.ends_with('\r') {
                            line.pop();
                        }
                        if tx.send(Msg::Line(std::mem::take(&mut line))).is_err() {
                            return; // UI gone
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(Msg::Eof);
        })
        .expect("failed to spawn stdin reader thread")
}

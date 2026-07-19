//! Stdin reader thread: turns blocking line reads into channel messages so
//! the UI never blocks on I/O. A bounded channel applies backpressure to the
//! producer; the ring buffer in the UI caps total memory.

use std::io::{self, BufRead};
use std::thread;

use crossbeam_channel::Sender;

pub enum Msg {
    /// One line of input (terminators stripped, control sequences removed).
    Line(String),
    /// stdin reached EOF: the stream is now "static".
    Eof,
    /// Reading stdin failed; no more input will arrive.
    Err(String),
}

/// Spawn the reader thread. It exits after sending `Eof`, or immediately if
/// the UI hung up (channel closed on quit).
pub fn spawn(tx: Sender<Msg>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("logtui-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut lock = stdin.lock();
            let mut raw = Vec::new();
            loop {
                raw.clear();
                // Read raw bytes, not UTF-8 lines: real log streams routinely
                // contain invalid sequences, and one bad byte must not end the
                // stream.
                match lock.read_until(b'\n', &mut raw) {
                    Ok(0) => break, // EOF
                    Ok(_) => {
                        if tx.send(Msg::Line(sanitize_line(&raw))).is_err() {
                            return; // UI gone
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Msg::Err(e.to_string()));
                        return;
                    }
                }
            }
            let _ = tx.send(Msg::Eof);
        })
        .expect("failed to spawn stdin reader thread")
}

/// One raw input record → clean display line: strip the line terminator,
/// decode as UTF-8 (invalid bytes become U+FFFD), drop control sequences.
fn sanitize_line(mut raw: &[u8]) -> String {
    while let [rest @ .., b'\n' | b'\r'] = raw {
        raw = rest;
    }
    strip_controls(&String::from_utf8_lossy(raw))
}

/// Remove terminal escape sequences and control characters from a log line.
///
/// Sequences handled:
///   - CSI:  ESC `[` ... final-byte (0x40–0x7e)  e.g. color, cursor, erase
///   - OSC:  ESC `]` ... BEL or ST               e.g. window title, hyperlinks
///   - DCS/PM/APC: ESC `P`/`X`/`^`/`_` ... ST   e.g. Terminology shell integration
///   - Fe two-char: ESC + single byte             e.g. ESC M (reverse index)
///   - C0 controls (0x00–0x1f) except horizontal tab
///   - DEL (0x7f) and C1 controls (U+0080–U+009F)
///
/// Tab is preserved; all other control characters are silently dropped so that
/// no attacker-controlled log content can manipulate the terminal emulator.
fn strip_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.peek().copied() {
                Some('[') => {
                    // CSI sequence: ESC [ <param bytes> <intermediate bytes> <final byte>
                    // Final byte is in 0x40–0x7e (@–~).
                    chars.next();
                    for c2 in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c2) {
                            break;
                        }
                    }
                }
                Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => {
                    // OSC / DCS / PM / APC: terminated by BEL (0x07) or ST (ESC \).
                    chars.next();
                    loop {
                        match chars.next() {
                            None | Some('\x07') => break,
                            Some('\x1b') => {
                                if chars.peek().copied() == Some('\\') {
                                    chars.next();
                                }
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                Some(_) => {
                    // Two-character Fe sequence (ESC + one byte): skip both.
                    chars.next();
                }
                None => {} // trailing bare ESC — drop it
            },
            // Keep tab; drop all other C0 controls, DEL, and C1 controls.
            '\t' => out.push('\t'),
            c if c.is_control() => {}
            c if ('\u{80}'..='\u{9f}').contains(&c) => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{sanitize_line, strip_controls};

    #[test]
    fn sanitize_strips_terminators_and_decodes_lossily() {
        assert_eq!(sanitize_line(b"hello\n"), "hello");
        assert_eq!(sanitize_line(b"hello\r\n"), "hello");
        assert_eq!(sanitize_line(b"no newline"), "no newline");
        // Invalid UTF-8 must not kill the stream: bad bytes become U+FFFD.
        assert_eq!(sanitize_line(b"bad \xff byte\n"), "bad \u{fffd} byte");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(strip_controls("hello world"), "hello world");
        assert_eq!(strip_controls("tab\there"), "tab\there");
    }

    #[test]
    fn strips_csi_color_codes() {
        assert_eq!(strip_controls("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_controls("\x1b[1;32mbold green\x1b[m"), "bold green");
    }

    #[test]
    fn strips_osc_title_sequence() {
        // OSC 0 ; title BEL  — window title injection
        assert_eq!(strip_controls("\x1b]0;evil title\x07text"), "text");
        // OSC terminated by ST (ESC \)
        assert_eq!(strip_controls("\x1b]0;title\x1b\\after"), "after");
    }

    #[test]
    fn strips_dcs_sequence() {
        assert_eq!(strip_controls("\x1bPdata\x1b\\visible"), "visible");
    }

    #[test]
    fn strips_fe_two_char_sequences() {
        // ESC M = reverse index
        assert_eq!(strip_controls("\x1bMtext"), "text");
    }

    #[test]
    fn strips_c0_controls_except_tab() {
        assert_eq!(strip_controls("a\x00b\x07c\x08d"), "abcd");
        assert_eq!(strip_controls("col1\tcol2"), "col1\tcol2");
    }

    #[test]
    fn strips_c1_controls() {
        // U+0085 (NEL), U+009B (CSI as single byte)
        assert_eq!(strip_controls("a\u{85}b\u{9b}c"), "abc");
    }

    #[test]
    fn keystroke_injection_via_terminal_query_blocked() {
        // ESC [ c = device attributes query; the terminal responds by injecting
        // bytes into stdin. After stripping, the ESC sequence is gone.
        let injected = "look at this\x1b[cmalicious";
        assert_eq!(strip_controls(injected), "look at thismalicious");
    }
}

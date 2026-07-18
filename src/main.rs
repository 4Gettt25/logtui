//! logtui — pipe anything into an interactive TUI viewer.
//!
//!   docker compose logs -f web | logtui
//!   cat bigfile.log | logtui --buffer-size 50000
//!   journalctl -f | logtui --regex
//!
//! Architecture: a reader thread pumps stdin lines into a bounded channel;
//! the UI thread drains it each tick into a capped ring buffer, keeps a
//! filtered *view* over that buffer, and renders with ratatui.

mod app;
mod buffer;
mod filter;
mod reader;
mod ui;

use std::io::{self, IsTerminal};
use std::time::Duration;

use clap::Parser;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use filter::MatchMode;

#[derive(Parser, Debug)]
#[command(
    name = "logtui",
    version,
    about = "Pipe any command's output into an interactive TUI viewer",
    long_about = "logtui treats input as opaque lines (no timestamp/log-format assumptions).\n\
                  It works with finite input (cat, find — read to EOF, then browse) and with\n\
                  infinite streams (tail -f, journalctl -f, docker compose logs -f)."
)]
struct Cli {
    /// Ring buffer capacity: maximum lines kept in memory (oldest are dropped)
    #[arg(long, default_value_t = 10_000, value_name = "N")]
    buffer_size: usize,

    /// Don't auto-scroll on new lines: start at the top and stay put
    /// (browse what has been buffered so far, even while input streams in)
    #[arg(long)]
    no_follow: bool,

    /// Start in regex filter mode instead of fuzzy ('r' toggles at runtime)
    #[arg(long)]
    regex: bool,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();

    if io::stdin().is_terminal() {
        eprintln!("logtui: nothing to read — pipe input in, e.g.:");
        eprintln!("  tail -f app.log | logtui");
        eprintln!("  find / -name '*.rs' | logtui");
        std::process::exit(2);
    }

    // Reader thread -> bounded channel (backpressure on fast producers).
    let (tx, rx) = crossbeam_channel::bounded::<reader::Msg>(8192);
    reader::spawn(tx);

    // Restore the terminal even on panic.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    // Crossterm drives the terminal via /dev/tty (stdin is the data pipe), so
    // a controlling terminal is required. Fail with a clear message, not an
    // opaque ENXIO.
    if let Err(e) = setup_terminal() {
        eprintln!("logtui: cannot access the controlling terminal ({e}).");
        eprintln!("logtui must run in an interactive terminal; stdin stays free for the pipe.");
        std::process::exit(2);
    }
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let mode = if cli.regex {
        MatchMode::Regex
    } else {
        MatchMode::Fuzzy
    };
    let mut app = App::new(cli.buffer_size, !cli.no_follow, mode);

    let result = run(&mut terminal, &mut app, &rx);

    restore_terminal();
    result
}

fn setup_terminal() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    rx: &crossbeam_channel::Receiver<reader::Msg>,
) -> io::Result<()> {
    let tick = Duration::from_millis(33); // ~30 fps cap, keeps UI snappy
    loop {
        // Drain everything the reader thread has for us this tick.
        let mut got_data = false;
        while let Ok(msg) = rx.try_recv() {
            got_data = true;
            match msg {
                reader::Msg::Line(line) => app.push_line(line),
                reader::Msg::Eof => app.set_eof(),
            }
        }
        if got_data {
            app.after_batch();
        }

        if event::poll(tick)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.on_key(key),
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => app.scroll_lines(-3),
                    MouseEventKind::ScrollDown => app.scroll_lines(3),
                    _ => {}
                },
                Event::Resize(_, _) => {} // next draw picks up the new size
                _ => {}
            }
        }

        terminal.draw(|f| ui::draw(f, app))?;

        if app.should_quit {
            return Ok(());
        }
    }
}

//! logtui — pipe anything into an interactive TUI viewer.
//!
//!   docker compose logs -f web | logtui
//!   cat bigfile.log | logtui --buffer-size 50000
//!   journalctl -f | logtui --regex
//!
//! Architecture: a reader thread pumps stdin lines into a bounded channel;
//! the UI thread drains it each tick into a capped ring buffer (hot tail
//! cache) while archiving every line to a spill file, keeps a filtered
//! *view* over the full history, and renders with ratatui.

mod app;
mod buffer;
mod filter;
mod reader;
mod spill;
mod ui;
mod upstream;

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
    /// In-memory cache size in lines. Older lines spill to a temp file and
    /// stay scrollable/filterable; RAM stays bounded regardless of input size
    #[arg(long, default_value_t = 10_000, value_name = "N")]
    buffer_size: usize,

    /// Don't write history to a temp file: keep only --buffer-size lines
    /// (older lines are gone for good, but nothing ever touches the disk)
    #[arg(long)]
    no_spill: bool,

    /// Don't auto-scroll on new lines: start at the top and stay put
    /// (browse what has been buffered so far, even while input streams in)
    #[arg(long)]
    no_follow: bool,

    /// Start in regex filter mode instead of fuzzy ('r' toggles at runtime)
    #[arg(long)]
    regex: bool,

    /// On quit, leave the upstream pipeline command running instead of
    /// terminating it (you may need Ctrl+C to get your prompt back)
    #[arg(long)]
    no_kill_upstream: bool,
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

    // Build the app before touching the terminal: creating the spill file can
    // fail, and the error should print to a normal (cooked) screen.
    let mode = if cli.regex {
        MatchMode::Regex
    } else {
        MatchMode::Fuzzy
    };
    let mut app = match App::new(cli.buffer_size, !cli.no_follow, mode, !cli.no_spill) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("logtui: cannot create the spill file ({e}).");
            eprintln!("Retry with --no-spill to run without on-disk history.");
            std::process::exit(2);
        }
    };

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

    let result = run(&mut terminal, &mut app, &rx);

    restore_terminal();

    // The producer keeps running (and the shell keeps waiting) until its next
    // write hits the broken pipe — terminate it so quitting returns the prompt.
    if !cli.no_kill_upstream {
        upstream::terminate();
    }
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
    // Bound the per-tick drain: a producer that outruns us (fast stream +
    // per-line filter matching) must not pin us in the drain loop and starve
    // input handling and rendering.
    const MAX_DRAIN_PER_TICK: usize = 8192;
    loop {
        let mut drained = 0;
        while drained < MAX_DRAIN_PER_TICK {
            match rx.try_recv() {
                Ok(reader::Msg::Line(line)) => app.push_line(line),
                Ok(reader::Msg::Eof) => app.set_eof(),
                Ok(reader::Msg::Err(e)) => app.set_read_error(e),
                Err(_) => break,
            }
            drained += 1;
        }
        if drained > 0 {
            app.after_batch();
        }

        // Hitting the cap means a backlog is waiting: don't block on the
        // event poll, clear it at full speed (drawing throttles the loop).
        let timeout = if drained == MAX_DRAIN_PER_TICK {
            Duration::ZERO
        } else {
            tick
        };
        if event::poll(timeout)? {
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

# logtui

Pipe any command's output into an interactive TUI viewer. Input is treated as
**opaque lines of text** — no log-format or timestamp assumptions.

```sh
docker compose logs -f web | logtui
cat bigfile.log            | logtui
find / -name "*.rs"        | logtui
journalctl -f              | logtui --regex
```

Works with **finite input** (`cat`, `find` — reads to EOF, then you browse a
static view) and **infinite streams** (`tail -f` — auto-follows new lines).

## Screenshots

| Follow mode (live stream) | Fuzzy filter `err` | Regex `api/(users\|orders)` |
| --- | --- | --- |
| ![follow](docs/follow.png) | ![filter](docs/filter.png) | ![regex](docs/regex.png) |

## Features

- **Responsive under load** — a dedicated reader thread pumps stdin into a
  bounded channel; the UI drains it on a ~30 fps tick and never blocks on I/O.
- **Full scrollback, bounded RAM** — the last `--buffer-size` lines (default
  10,000) stay in memory; everything older spills to a temp file with a sparse
  index, so you can scroll to line 1 and filter across the entire history no
  matter how much has streamed in. `--no-spill` disables the temp file (e.g.
  for sensitive input) and restores the old drop-oldest behavior.
- **Follow mode** — auto-scrolls to the newest line until you scroll up;
  `G` (or scrolling back to the bottom) resumes. `--no-follow` starts parked
  at the top even while input is still streaming.
- **Live filter** — `/` opens the search bar and filters as you type:
  - *fuzzy* (default, subsequence matching via `fuzzy-matcher`, smart-case)
  - *regex* (`--regex` to start there, `r` / `Ctrl-r` to toggle)
  - *substring fallback* — an invalid regex degrades to a literal substring
    match instead of showing nothing
- **Match highlighting** — the exact matched characters are highlighted in
  every visible line (fuzzy char positions, substring spans, or regex spans).
- **Filtering is a view** — the buffer is never mutated; `Esc` restores the
  full stream instantly, and lines arriving while a filter is active are
  matched live. The view stores stable absolute line numbers, so it stays
  correct even as the ring buffer evicts old lines.
- **Robust input** — stdin is read as raw bytes: invalid UTF-8 becomes `�`
  instead of truncating the stream, and a real read error is surfaced in the
  status bar instead of masquerading as EOF.
- **Status bar** — buffered/dropped line counts, active filter + mode, match
  count, `● LIVE` / `■ EOF` / `■ READ ERROR` input state, follow indicator,
  position.
- **Graceful terminal handling** — raw mode + alternate screen restored on
  quit *and* on panic; resize just works; clear error if there's no
  controlling terminal.

## Keybindings

| Key | Action |
| --- | --- |
| `j` / `k` / `↑` / `↓` | scroll one line |
| `PageUp` / `PageDown`, `Ctrl-u` / `Ctrl-d` | page / half-page |
| `g` / `G` (or `Home` / `End`) | jump to top / bottom (`G` resumes follow) |
| `/` | open search bar (filters in real time) |
| `Enter` | confirm filter, back to scroll mode |
| `Esc` | clear filter / close search bar |
| `n` / `N` | next / previous match (wraps) |
| `r` | toggle fuzzy ⇄ regex (`Ctrl-r` inside the search bar) |
| `Ctrl-a` / `Ctrl-e` / `Ctrl-u` / `Ctrl-w` | in the search bar: start / end / clear to start / delete word |
| `h` / `l` / `←` / `→` | horizontal scroll for long lines |
| mouse wheel | scroll |
| `q` / `Ctrl-c` | quit |

## CLI flags

```
--buffer-size <N>   in-memory cache size in lines; older lines spill to disk
                    and stay scrollable/filterable [default: 10000]
--no-spill          no temp file: keep only --buffer-size lines, older lines
                    are gone for good (nothing ever touches the disk)
--no-follow         don't auto-scroll; start at the top and stay put
--regex             start in regex filter mode instead of fuzzy
```

## Architecture

```
stdin ──▶ reader thread ──▶ bounded crossbeam channel ──▶ UI thread
              (blocking I/O)        (backpressure)            │
                                                              ▼
   ratatui render  ◀──  App state  ◀──  RingBuffer (hot tail cache, RAM)
   (ui.rs)                    (app.rs)        (buffer.rs)
                                   │               ▼
                                   │     Spill file (full history on disk,
                                   │     sparse offset index — spill.rs)
                                   ▼
                          Filter view — Vec<abs line numbers>
                          fuzzy / substring / regex (filter.rs)
```

- `reader.rs` — stdin reader thread, sends `Line` / `Eof` messages.
- `buffer.rs` — `RingBuffer` with eviction counting and absolute numbering.
- `spill.rs` — append-only temp-file archive of every line, sparse-indexed
  so any absolute line number can be fetched; deleted on exit.
- `filter.rs` — `Filter` (compiled per query/mode) → highlight char indices.
- `app.rs` — state machine: view, selection, follow, keybindings.
- `ui.rs` — ratatui rendering (gutter, highlight, status/search bars).
- `main.rs` — clap CLI, terminal setup/teardown, ~30 fps event loop.

## Build & test

```sh
cargo build --release        # binary: target/release/logtui
cargo test                   # unit tests (buffer, filter, app state machine)
python3 tests/pty_harness.py # integration: drives the real binary in a pty
```

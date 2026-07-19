//! Application state: buffer + filter view + scroll/selection + input mode.

use std::borrow::Cow;
use std::io;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::buffer::RingBuffer;
use crate::filter::{Filter, MatchMode};
use crate::spill::Spill;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    /// Producer still running (no EOF yet).
    Live,
    /// stdin closed — finite input fully read.
    Eof,
    /// Reading stdin failed — the stream ended abnormally.
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Normal,
    Search,
}

pub struct App {
    buf: RingBuffer,
    /// Full-history archive on disk; `None` with `--no-spill` (then only the
    /// ring's contents are reachable, as before).
    spill: Option<Spill>,

    // filter state
    query: String,
    cursor: usize, // cursor position within `query`, in chars
    requested_mode: MatchMode,
    filter: Option<Filter>,
    fuzzy: SkimMatcherV2,

    // view: None = identity (all lines); Some = absolute numbers of matches
    view: Option<Vec<u64>>,
    selected: usize, // index into the *view*
    scroll: usize,   // topmost visible view row
    viewport: usize, // rows visible in the list area (set by renderer)
    hscroll: u16,

    follow: bool,
    input_state: InputState,
    read_error: Option<String>,
    focus: Focus,
    pub should_quit: bool,
}

impl App {
    pub fn new(buffer_cap: usize, follow: bool, mode: MatchMode, spill: bool) -> io::Result<Self> {
        Ok(Self {
            buf: RingBuffer::new(buffer_cap.max(1)),
            spill: if spill { Some(Spill::new()?) } else { None },
            query: String::new(),
            cursor: 0,
            requested_mode: mode,
            filter: None,
            fuzzy: SkimMatcherV2::default().smart_case(),
            view: None,
            selected: 0,
            scroll: 0,
            viewport: 20,
            hscroll: 0,
            follow,
            input_state: InputState::Live,
            read_error: None,
            focus: Focus::Normal,
            should_quit: false,
        })
    }

    // ---------------------------------------------------------------- input

    pub fn push_line(&mut self, line: String) {
        if let Some(sp) = &mut self.spill {
            sp.append(&line);
        }
        let abs = self.buf.push(line);
        if let Some(filter) = &self.filter {
            // Live-filter new arrivals: matching lines join the view.
            let matches = self
                .buf
                .get_abs(abs)
                .is_some_and(|t| filter.match_indices(&self.fuzzy, t).is_some());
            if matches {
                if let Some(view) = &mut self.view {
                    view.push(abs);
                }
            }
        }
    }

    /// Called once per event-loop tick after a batch of new lines.
    pub fn after_batch(&mut self) {
        if self.follow {
            self.snap_to_bottom();
        }
    }

    pub fn set_eof(&mut self) {
        self.input_state = InputState::Eof;
    }

    pub fn set_read_error(&mut self, msg: String) {
        self.input_state = InputState::Error;
        self.read_error = Some(msg);
    }

    // ------------------------------------------------------------- accessors

    pub fn input_state(&self) -> InputState {
        self.input_state
    }

    pub fn read_error(&self) -> Option<&str> {
        self.read_error.as_deref()
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn follow(&self) -> bool {
        self.follow
    }

    pub fn buffer(&self) -> &RingBuffer {
        &self.buf
    }

    /// Lines ever received (the ring counts them all, even after eviction).
    pub fn total_lines(&self) -> u64 {
        self.buf.dropped() + self.buf.len() as u64
    }

    pub fn spill_enabled(&self) -> bool {
        self.spill.is_some()
    }

    pub fn spill_error(&self) -> Option<&str> {
        self.spill.as_ref()?.error()
    }

    /// Line content by absolute number: from the ring if still hot, else from
    /// the spill file. `None` only when unavailable (no spill, or spill error).
    pub fn line(&self, abs: u64) -> Option<Cow<'_, str>> {
        if let Some(s) = self.buf.get_abs(abs) {
            return Some(Cow::Borrowed(s));
        }
        self.spill.as_ref()?.read_line(abs).map(Cow::Owned)
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn filter(&self) -> Option<&Filter> {
        self.filter.as_ref()
    }

    pub fn requested_mode(&self) -> MatchMode {
        self.requested_mode
    }

    pub fn hscroll(&self) -> u16 {
        self.hscroll
    }

    pub fn view_len(&self) -> usize {
        self.view
            .as_ref()
            .map_or(self.total_lines() as usize, Vec::len)
    }

    /// First reachable view row. With the spill the whole history is live
    /// (0); without it, rows evicted from the ring are gone for good.
    fn min_row(&self) -> usize {
        if self.view.is_some() || self.spill.is_some() {
            0
        } else {
            self.buf.dropped() as usize
        }
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Absolute line number for a view row. Without a filter, view rows *are*
    /// absolute numbers: the view spans the entire history, not just the ring.
    pub fn abs_at(&self, row: usize) -> Option<u64> {
        match &self.view {
            Some(v) => v.get(row).copied(),
            None => ((row as u64) < self.total_lines()).then_some(row as u64),
        }
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
    }

    /// Adjust `scroll` so `selected` is inside the viewport, and keep both
    /// within the reachable row range.
    pub fn ensure_visible(&mut self) {
        let h = self.viewport;
        let len = self.view_len();
        let min = self.min_row().min(len.saturating_sub(1));
        self.selected = self.selected.max(min);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + h {
            self.scroll = self.selected + 1 - h;
        }
        let max_scroll = len.saturating_sub(h).max(min);
        self.scroll = self.scroll.clamp(min, max_scroll);
    }

    // ------------------------------------------------------------ navigation

    fn snap_to_bottom(&mut self) {
        self.selected = self.view_len().saturating_sub(1);
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.view_len();
        if len == 0 {
            return;
        }
        let new = self.selected as isize + delta;
        self.selected = new.clamp(self.min_row() as isize, len as isize - 1) as usize;
        // Standard tail behaviour: scrolling off the bottom pauses follow,
        // scrolling back onto it (or G) resumes it.
        self.follow = self.selected == len - 1;
    }

    /// `n` / `N`: cycle through matches (wraps). Only meaningful when a
    /// filter is active — the view then *is* the match list.
    fn step_match(&mut self, dir: isize) {
        let len = self.view_len();
        if len == 0 || self.filter.is_none() {
            return;
        }
        let n = len as isize;
        self.selected = ((self.selected as isize + dir).rem_euclid(n)) as usize;
        self.follow = self.selected == len - 1;
    }

    pub fn scroll_lines(&mut self, delta: isize) {
        self.move_by(delta);
    }

    // ----------------------------------------------------------- key handling

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match self.focus {
            Focus::Normal => self.on_normal_key(key),
            Focus::Search => self.on_search_key(key),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let half = (self.viewport / 2) as isize;
        let page = self.viewport as isize;
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('/') => {
                self.focus = Focus::Search;
                self.cursor = self.query.chars().count();
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by(half)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_by(-half)
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.selected = self.min_row();
                self.follow = false;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.snap_to_bottom();
                self.follow = true;
            }
            KeyCode::Char('n') => self.step_match(1),
            KeyCode::Char('N') => self.step_match(-1),
            KeyCode::Char('r') => self.toggle_mode(),
            KeyCode::Char('h') | KeyCode::Left => {
                self.hscroll = self.hscroll.saturating_sub(4)
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.hscroll = self.hscroll.saturating_add(8)
            }
            KeyCode::Esc => self.clear_filter(),
            _ => {}
        }
    }

    fn on_search_key(&mut self, key: KeyEvent) {
        // Plain Ctrl only: Ctrl+Alt together is how Windows reports AltGr,
        // which produces text (e.g. '@' on German layouts), not a command.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter => self.focus = Focus::Normal,
            KeyCode::Esc => {
                self.clear_filter();
                self.focus = Focus::Normal;
            }
            KeyCode::Char('r') if ctrl => self.toggle_mode(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.query.chars().count(),
            KeyCode::Char('u') if ctrl => {
                // Delete from the start of the query to the cursor.
                let mut chars: Vec<char> = self.query.chars().collect();
                chars.drain(..self.cursor.min(chars.len()));
                self.cursor = 0;
                self.query = chars.into_iter().collect();
                self.refresh_view();
            }
            KeyCode::Char('w') if ctrl => {
                // Delete the word before the cursor.
                let mut chars: Vec<char> = self.query.chars().collect();
                let end = self.cursor.min(chars.len());
                let mut start = end;
                while start > 0 && chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                chars.drain(start..end);
                self.cursor = start;
                self.query = chars.into_iter().collect();
                self.refresh_view();
            }
            KeyCode::Char(c) if is_text_input(key.modifiers) => {
                let mut chars: Vec<char> = self.query.chars().collect();
                chars.insert(self.cursor.min(chars.len()), c);
                self.cursor = (self.cursor + 1).min(chars.len());
                self.query = chars.into_iter().collect();
                self.refresh_view();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let mut chars: Vec<char> = self.query.chars().collect();
                    chars.remove(self.cursor - 1);
                    self.cursor -= 1;
                    self.query = chars.into_iter().collect();
                    self.refresh_view();
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.query.chars().count())
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.query.chars().count(),
            _ => {}
        }
    }

    // -------------------------------------------------------------- filtering

    fn toggle_mode(&mut self) {
        self.requested_mode = match self.requested_mode {
            MatchMode::Regex => MatchMode::Fuzzy,
            _ => MatchMode::Regex,
        };
        if !self.query.is_empty() {
            self.refresh_view();
        }
    }

    fn clear_filter(&mut self) {
        if self.query.is_empty() && self.filter.is_none() {
            return;
        }
        self.query.clear();
        self.cursor = 0;
        self.filter = None;
        self.view = None;
        let len = self.view_len();
        if len == 0 {
            self.selected = 0;
        } else if self.follow {
            self.selected = len - 1;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    /// Rebuild the filter and rescan the history (full spill if available,
    /// otherwise just the ring). Nothing is ever mutated by matching.
    fn refresh_view(&mut self) {
        if self.query.is_empty() {
            self.clear_filter();
            return;
        }
        let filter = Filter::new(self.query.clone(), self.requested_mode);
        let mut view = Vec::new();
        match &self.spill {
            Some(sp) => sp.scan(|abs, line| {
                if filter.match_indices(&self.fuzzy, line).is_some() {
                    view.push(abs);
                }
            }),
            None => {
                for (abs, line) in self.buf.iter_abs() {
                    if filter.match_indices(&self.fuzzy, line).is_some() {
                        view.push(abs);
                    }
                }
            }
        }
        self.filter = Some(filter);
        self.view = Some(view);
        let len = self.view_len();
        if len == 0 {
            self.selected = 0;
        } else if self.follow {
            self.selected = len - 1;
        } else {
            self.selected = self.selected.min(len - 1);
        }
    }

    /// Highlight indices for one visible line (renderer recomputes per frame —
    /// only for the handful of rows on screen, so it's cheap). Takes the text
    /// the renderer already fetched so spilled lines aren't read twice.
    pub fn highlight_for_text(&self, text: &str) -> Option<Vec<usize>> {
        self.filter.as_ref()?.match_indices(&self.fuzzy, text)
    }
}

/// True when the modifier set still produces a text character: nothing,
/// Shift, or Ctrl+Alt together (AltGr on Windows). Bare Ctrl or bare Alt
/// means a command chord, not input.
fn is_text_input(mods: KeyModifiers) -> bool {
    let ca = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    ca.is_empty() || ca == KeyModifiers::CONTROL | KeyModifiers::ALT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(lines: &[&str], follow: bool) -> App {
        let mut app = App::new(100, follow, MatchMode::Fuzzy, true).unwrap();
        for l in lines {
            app.push_line(l.to_string());
        }
        app.after_batch();
        app
    }

    #[test]
    fn follow_snaps_to_bottom_on_new_lines() {
        let mut app = app_with(&["a", "b"], true);
        assert_eq!(app.selected(), 1);
        app.push_line("c".into());
        app.after_batch();
        assert_eq!(app.selected(), 2);
        // scroll up -> follow off -> new lines no longer move selection
        app.scroll_lines(-1);
        assert!(!app.follow());
        app.push_line("d".into());
        app.after_batch();
        assert_eq!(app.selected(), 1); // still parked where the user scrolled to
        // G resumes
        app.on_key(KeyEvent::from(KeyCode::Char('G')));
        assert!(app.follow());
        assert_eq!(app.selected(), 3);
    }

    #[test]
    fn filter_view_and_match_navigation() {
        let mut app = app_with(&["error one", "ok", "error two", "fine"], false);
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "error".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.view_len(), 2);
        assert_eq!(app.abs_at(0), Some(0));
        assert_eq!(app.abs_at(1), Some(2));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Char('n')));
        assert_eq!(app.selected(), 1);
        app.on_key(KeyEvent::from(KeyCode::Char('n'))); // wraps
        assert_eq!(app.selected(), 0);
        app.on_key(KeyEvent::from(KeyCode::Esc)); // clear filter
        assert_eq!(app.view_len(), 4);
        assert!(app.filter().is_none());
    }

    #[test]
    fn search_editing_and_modifier_handling() {
        let mut app = app_with(&["one two three"], false);
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "one two".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        // Ctrl-modified chars are commands, not query input.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL));
        assert_eq!(app.query(), "one two");
        // AltGr (Ctrl+Alt) still types text, e.g. '@' on German layouts.
        app.on_key(KeyEvent::new(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(app.query(), "one two@");
        app.on_key(KeyEvent::from(KeyCode::Backspace));
        // Ctrl+W deletes the word before the cursor.
        app.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.query(), "one ");
        // Ctrl+U clears back to the start.
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.query(), "");
        assert_eq!(app.cursor(), 0);
    }

    #[test]
    fn read_error_sets_state() {
        let mut app = app_with(&["a"], true);
        app.set_read_error("boom".into());
        assert_eq!(app.input_state(), InputState::Error);
        assert_eq!(app.read_error(), Some("boom"));
    }

    #[test]
    fn spill_keeps_full_history_after_eviction() {
        let mut app = App::new(3, false, MatchMode::Fuzzy, true).unwrap();
        for l in ["l1", "l2", "l3", "l4", "l5"] {
            app.push_line(l.into());
        }
        // ring holds only the last 3, but every line stays reachable
        assert_eq!(app.buffer().dropped(), 2);
        assert_eq!(app.view_len(), 5);
        assert_eq!(app.abs_at(0), Some(0));
        assert_eq!(app.line(0).as_deref(), Some("l1")); // from the spill
        assert_eq!(app.line(4).as_deref(), Some("l5")); // from the ring
        // g reaches the very first line
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.selected(), 0);
    }

    #[test]
    fn filter_sees_evicted_lines_via_spill() {
        let mut app = App::new(3, false, MatchMode::Fuzzy, true).unwrap();
        for l in ["keep x", "drop", "keep y", "n1", "n2"] {
            app.push_line(l.into());
        }
        assert_eq!(app.buffer().get_abs(0), None); // "keep x" evicted
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "keep".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        // the full-history scan still finds both matches
        assert_eq!(app.view_len(), 2);
        assert_eq!(app.abs_at(0), Some(0));
        assert_eq!(app.abs_at(1), Some(2));
        assert_eq!(app.line(0).as_deref(), Some("keep x"));
    }

    #[test]
    fn no_spill_clamps_navigation_to_ring() {
        let mut app = App::new(3, false, MatchMode::Fuzzy, false).unwrap();
        for l in ["l1", "l2", "l3", "l4", "l5"] {
            app.push_line(l.into());
        }
        // rows for evicted lines exist in the count but are unreachable
        assert_eq!(app.view_len(), 5);
        assert_eq!(app.line(0), None);
        app.on_key(KeyEvent::from(KeyCode::Char('g')));
        assert_eq!(app.selected(), 2); // oldest line still in the ring
        app.scroll_lines(-10);
        assert_eq!(app.selected(), 2);
    }
}

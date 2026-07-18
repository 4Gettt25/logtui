//! Application state: buffer + filter view + scroll/selection + input mode.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::buffer::RingBuffer;
use crate::filter::{Filter, MatchMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    /// Producer still running (no EOF yet).
    Live,
    /// stdin closed — finite input fully read.
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Normal,
    Search,
}

pub struct App {
    buf: RingBuffer,

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
    focus: Focus,
    pub should_quit: bool,
}

impl App {
    pub fn new(buffer_cap: usize, follow: bool, mode: MatchMode) -> Self {
        Self {
            buf: RingBuffer::new(buffer_cap.max(1)),
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
            focus: Focus::Normal,
            should_quit: false,
        }
    }

    // ---------------------------------------------------------------- input

    pub fn push_line(&mut self, line: String) {
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

    // ------------------------------------------------------------- accessors

    pub fn input_state(&self) -> InputState {
        self.input_state
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
        self.view.as_ref().map_or_else(|| self.buf.len(), Vec::len)
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Absolute line number for a view row.
    pub fn abs_at(&self, row: usize) -> Option<u64> {
        match &self.view {
            Some(v) => v.get(row).copied(),
            None => self.buf.abs_of(row),
        }
    }

    pub fn set_viewport(&mut self, rows: usize) {
        self.viewport = rows.max(1);
    }

    /// Adjust `scroll` so `selected` is inside the viewport.
    pub fn ensure_visible(&mut self) {
        let h = self.viewport;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + h {
            self.scroll = self.selected + 1 - h;
        }
        let max_scroll = self.view_len().saturating_sub(h);
        self.scroll = self.scroll.min(max_scroll);
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
        self.selected = new.clamp(0, len as isize - 1) as usize;
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
                self.selected = 0;
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
        match key.code {
            KeyCode::Enter => self.focus = Focus::Normal,
            KeyCode::Esc => {
                self.clear_filter();
                self.focus = Focus::Normal;
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_mode()
            }
            KeyCode::Char(c) => {
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

    /// Rebuild the filter and rescan the buffer. The buffer is never mutated.
    fn refresh_view(&mut self) {
        if self.query.is_empty() {
            self.clear_filter();
            return;
        }
        let filter = Filter::new(self.query.clone(), self.requested_mode);
        let mut view = Vec::new();
        for (abs, line) in self.buf.iter_abs() {
            if filter.match_indices(&self.fuzzy, line).is_some() {
                view.push(abs);
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
    /// only for the handful of rows on screen, so it's cheap).
    pub fn highlight_for(&self, abs: u64) -> Option<Vec<usize>> {
        let filter = self.filter.as_ref()?;
        let line = self.buf.get_abs(abs)?;
        filter.match_indices(&self.fuzzy, line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(lines: &[&str], follow: bool) -> App {
        let mut app = App::new(100, follow, MatchMode::Fuzzy);
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
    fn view_survives_eviction() {
        let mut app = App::new(3, false, MatchMode::Fuzzy);
        for l in ["keep x", "drop", "keep y"] {
            app.push_line(l.into());
        }
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "keep".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(app.view_len(), 2);
        // push one more line: with cap=3 that evicts exactly "keep x" (abs 0)
        app.push_line("n1".into());
        assert_eq!(app.buffer().dropped(), 1);
        // view rows with evicted abs numbers are skipped by the renderer
        assert_eq!(app.view_len(), 2);
        assert_eq!(app.abs_at(0), Some(0)); // evicted; get_abs returns None
        assert_eq!(app.buffer().get_abs(0), None);
        assert_eq!(app.buffer().get_abs(2), Some("keep y"));
    }
}

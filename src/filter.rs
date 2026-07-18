//! Filter / match logic: fuzzy (default), plain substring, and regex.
//!
//! A [`Filter`] is rebuilt from scratch whenever the query or the requested
//! mode changes; matching itself is a pure view over the buffer and never
//! mutates it. Match results are returned as *char indices* so the renderer
//! can highlight the exact matched characters.

use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use regex::Regex;

/// Mode requested by the user (`r` toggles Fuzzy <-> Regex).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Fuzzy,
    Substring,
    Regex,
}

impl MatchMode {
    pub fn label(self) -> &'static str {
        match self {
            MatchMode::Fuzzy => "fuzzy",
            MatchMode::Substring => "substr",
            MatchMode::Regex => "regex",
        }
    }
}

/// A compiled, ready-to-run filter for one query string.
pub struct Filter {
    query: String,
    requested: MatchMode,
    effective: MatchMode,
    regex: Option<Regex>,
}

impl Filter {
    pub fn new(query: String, requested: MatchMode) -> Self {
        let (effective, regex) = match requested {
            MatchMode::Regex => match Regex::new(&query) {
                Ok(re) => (MatchMode::Regex, Some(re)),
                // Fallback: an uncompilable regex degrades to plain substring
                // matching instead of showing nothing.
                Err(_) => (MatchMode::Substring, None),
            },
            mode => (mode, None),
        };
        Self {
            query,
            requested,
            effective,
            regex,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn effective_mode(&self) -> MatchMode {
        self.effective
    }

    /// True when the user asked for regex but it failed to compile.
    pub fn is_regex_fallback(&self) -> bool {
        self.requested == MatchMode::Regex && self.effective == MatchMode::Substring
    }

    /// Char indices to highlight if `line` matches, else None.
    pub fn match_indices(&self, fuzzy: &SkimMatcherV2, line: &str) -> Option<Vec<usize>> {
        if self.query.is_empty() {
            return None;
        }
        match self.effective {
            MatchMode::Fuzzy => fuzzy
                .fuzzy_indices(line, &self.query)
                .map(|(_score, idx)| idx),
            MatchMode::Substring => substring_indices(line, &self.query),
            MatchMode::Regex => {
                let re = self.regex.as_ref()?;
                let mut idx = Vec::new();
                for m in re.find_iter(line) {
                    let start = line[..m.start()].chars().count();
                    let len = m.as_str().chars().count();
                    idx.extend(start..start + len);
                }
                (!idx.is_empty()).then_some(idx)
            }
        }
    }
}

/// Smart-case substring match: case-insensitive unless the query contains an
/// uppercase character. Returns char indices of every (non-overlapping) hit.
pub fn substring_indices(line: &str, query: &str) -> Option<Vec<usize>> {
    let sensitive = query.chars().any(char::is_uppercase);
    let line_chars: Vec<char> = line.chars().collect();
    let qchars: Vec<char> = query.chars().collect();
    let qlen = qchars.len();
    if qlen == 0 || qlen > line_chars.len() {
        return None;
    }
    let norm = |c: char| {
        if sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        }
    };
    let mut idx = Vec::new();
    let mut i = 0;
    while i + qlen <= line_chars.len() {
        if (0..qlen).all(|j| norm(line_chars[i + j]) == norm(qchars[j])) {
            idx.extend(i..i + qlen);
            i += qlen;
        } else {
            i += 1;
        }
    }
    (!idx.is_empty()).then_some(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fuzzy() -> SkimMatcherV2 {
        SkimMatcherV2::default().smart_case()
    }

    #[test]
    fn substring_smart_case() {
        assert_eq!(substring_indices("Error error", "error"), Some(vec![0,1,2,3,4,6,7,8,9,10]));
        assert_eq!(substring_indices("Error error", "Error"), Some(vec![0,1,2,3,4]));
        assert_eq!(substring_indices("hello", "xyz"), None);
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        let f = Filter::new("err".into(), MatchMode::Fuzzy);
        assert!(f.match_indices(&fuzzy(), "2024-01-01 ERRor happened").is_some());
        assert!(f.match_indices(&fuzzy(), "completely unrelated").is_none());
    }

    #[test]
    fn regex_and_fallback() {
        let f = Filter::new(r"er+r".into(), MatchMode::Regex);
        assert_eq!(f.effective_mode(), MatchMode::Regex);
        assert!(f.match_indices(&fuzzy(), "an errror here").is_some());

        let bad = Filter::new("([".into(), MatchMode::Regex);
        assert!(bad.is_regex_fallback());
        assert_eq!(bad.effective_mode(), MatchMode::Substring);
        // falls back to literal substring
        assert!(bad.match_indices(&fuzzy(), "literal ([ in text").is_some());
    }
}

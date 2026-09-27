//! Tracks IME preview text and committed text.

use std::ops::Range;

/// IME preview state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImeState {
    preedit: Option<String>,
}

impl ImeState {
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.as_deref()
    }

    /// Returns the preview range in UTF-16 code units.
    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.preedit
            .as_ref()
            .map(|text| 0..text.encode_utf16().count())
    }

    /// Sets IME preview text. An empty string clears the preview.
    pub fn mark(&mut self, text: &str) {
        self.preedit = (!text.is_empty()).then(|| text.to_owned());
    }

    /// Commits non-empty text and clears the preview.
    pub fn commit(&mut self, text: &str) -> Option<String> {
        self.preedit = None;
        (!text.is_empty()).then(|| text.to_owned())
    }

    pub fn unmark(&mut self) {
        self.preedit = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_then_commit_replaces_preedit() {
        let mut state = ImeState::default();
        state.mark("zhong");
        assert_eq!(state.preedit(), Some("zhong"));
        state.mark("zhongg");
        assert_eq!(state.preedit(), Some("zhongg"));
        assert_eq!(state.commit("中"), Some("中".to_owned()));
        assert_eq!(state.preedit(), None);
    }

    #[test]
    fn marked_range_counts_utf16_units() {
        let mut state = ImeState::default();
        state.mark("zhong");
        assert_eq!(state.marked_range(), Some(0..5));
        state.mark("中");
        assert_eq!(state.marked_range(), Some(0..1));
        state.mark("🎉");
        assert_eq!(state.marked_range(), Some(0..2));
        state.unmark();
        assert_eq!(state.marked_range(), None);
    }
}

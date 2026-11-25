//! Searchable multi-column modal picker state for the `flyco` TUI.
//!
//! Powers `/sessions`, `/resume`, `/model`, `/provider`, `/repo`, `/machine`,
//! and `/resume-{codex,claude,agy,devin}` with one uniform keyboard model.

/// One item displayed in a modal picker.
#[derive(Debug, Clone)]
pub struct PickerEntry<T> {
    /// Underlying domain value returned when selected.
    pub value: T,
    /// Primary column (e.g. session title, model id, harness name).
    pub primary: String,
    /// Secondary column (e.g. repo slug, display name, workspace path).
    pub secondary: String,
    /// Status/tag badge rendered on the left or right (e.g. `ACTIVE`, `claude`).
    pub badge: String,
    /// Trailing column (e.g. timestamp, size/price, effort summary).
    pub trailing: String,
    /// Extra text included in filter matching (e.g. full UUID).
    pub search_extra: String,
}

/// Interactive filterable list state for a centered modal picker.
#[derive(Debug, Clone)]
pub struct Picker<T> {
    /// Modal heading (e.g. `"Active sessions"`).
    pub title: String,
    /// Live filter text typed by the user.
    pub query: String,
    /// All candidate items in their initial sort order.
    items: Vec<PickerEntry<T>>,
    /// Indices into [`Self::items`] matching [`Self::query`], ranked by score.
    filtered: Vec<usize>,
    /// Cursor position inside [`Self::filtered`].
    pub selected: usize,
    /// Message displayed when `items` is empty.
    pub empty_label: String,
}

impl<T> Picker<T> {
    /// Creates a new picker populated with `items` and an optional initial filter.
    #[must_use]
    pub fn new(
        title: impl Into<String>,
        items: Vec<PickerEntry<T>>,
        initial_query: &str,
        empty_label: impl Into<String>,
    ) -> Self {
        let mut picker = Self {
            title: title.into(),
            query: initial_query.trim().to_owned(),
            items,
            filtered: Vec::new(),
            selected: 0,
            empty_label: empty_label.into(),
        };
        picker.recompute();
        picker
    }

    /// Number of currently visible (filtered) entries.
    #[must_use]
    pub const fn visible_len(&self) -> usize {
        self.filtered.len()
    }

    /// Total unfiltered item count.
    #[must_use]
    pub const fn total_len(&self) -> usize {
        self.items.len()
    }

    /// Iterates visible entries in ranked order.
    pub fn visible(&self) -> impl Iterator<Item = &PickerEntry<T>> {
        self.filtered.iter().map(|&idx| &self.items[idx])
    }

    /// Appends one character to the filter query.
    pub fn push_char(&mut self, ch: char) {
        self.query.push(ch);
        self.recompute();
    }

    /// Removes the last character from the filter query.
    pub fn pop_char(&mut self) {
        if self.query.pop().is_some() {
            self.recompute();
        }
    }

    /// Clears the filter query.
    pub fn clear_query(&mut self) {
        if !self.query.is_empty() {
            self.query.clear();
            self.recompute();
        }
    }

    /// Moves selection up by one row (wrapping to the bottom).
    pub const fn move_up(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        if self.selected == 0 {
            self.selected = self.filtered.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    /// Moves selection down by one row (wrapping to the top).
    pub const fn move_down(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
    }

    /// Moves selection up by `step` rows without wrapping.
    pub const fn page_up(&mut self, step: usize) {
        self.selected = self.selected.saturating_sub(step);
    }

    /// Moves selection down by `step` rows without wrapping.
    pub fn page_down(&mut self, step: usize) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else {
            self.selected = (self.selected + step).min(self.filtered.len() - 1);
        }
    }

    /// Removes and returns the selected item's value, if any entry is visible.
    pub fn take_selected(&mut self) -> Option<T> {
        let &idx = self.filtered.get(self.selected)?;
        Some(self.items.swap_remove(idx).value)
    }

    fn recompute(&mut self) {
        let tokens: Vec<String> = self
            .query
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        if tokens.is_empty() {
            self.filtered = (0..self.items.len()).collect();
            self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
            return;
        }
        let mut scored: Vec<(u32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| score_entry(item, &tokens).map(|score| (score, idx)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        self.filtered = scored.into_iter().map(|(_, idx)| idx).collect();
        self.selected = 0;
    }
}

fn score_entry<T>(item: &PickerEntry<T>, tokens: &[String]) -> Option<u32> {
    let primary = item.primary.to_ascii_lowercase();
    let secondary = item.secondary.to_ascii_lowercase();
    let badge = item.badge.to_ascii_lowercase();
    let extra = item.search_extra.to_ascii_lowercase();
    let mut score = 0_u32;
    for token in tokens {
        if primary == *token {
            score += 100;
        } else if primary.starts_with(token) {
            score += 60;
        } else if primary.contains(token) {
            score += 40;
        } else if secondary.contains(token) {
            score += 25;
        } else if badge.contains(token) || extra.contains(token) {
            score += 15;
        } else {
            return None;
        }
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, primary: &str, secondary: &str) -> PickerEntry<String> {
        PickerEntry {
            value: id.to_owned(),
            primary: primary.to_owned(),
            secondary: secondary.to_owned(),
            badge: String::new(),
            trailing: String::new(),
            search_extra: id.to_owned(),
        }
    }

    #[test]
    fn filters_and_ranks_exact_and_prefix_hits() {
        let mut picker = Picker::new(
            "Models",
            vec![
                entry("m1", "claude-opus-4-7", "Anthropic"),
                entry("m2", "swe-2-1", "Devin"),
                entry("m3", "swe-2-1-lightning", "Devin"),
            ],
            "swe-2-1",
            "none",
        );
        assert_eq!(picker.visible_len(), 2);
        assert_eq!(picker.take_selected().as_deref(), Some("m2"));
    }
}

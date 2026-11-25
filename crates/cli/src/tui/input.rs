//! UTF-8 / CJK-aware composer input editor with history navigation.

use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// Maximum prompt history entries retained in memory.
const MAX_HISTORY: usize = 200;

/// Single-line / pasted-text composer state for the `flyco` TUI.
#[derive(Debug, Default, Clone)]
pub struct Composer {
    buffer: String,
    /// Byte offset of the cursor inside [`Self::buffer`] (always on a char boundary).
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    saved_draft: String,
}

impl Composer {
    /// Current text buffer.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.buffer
    }

    /// Whether the buffer contains no characters.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Display column width of the slice before the cursor.
    #[must_use]
    pub fn cursor_display_col(&self) -> usize {
        self.buffer[..self.cursor].width()
    }

    /// Inserts one character at the cursor.
    pub fn insert_char(&mut self, ch: char) {
        self.buffer.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_pos = None;
    }

    /// Inserts pasted text (normalizing CRLF to spaces so the single-row
    /// composer stays predictable).
    pub fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => self.insert_char(' '),
                other => self.insert_char(other),
            }
        }
    }

    /// Replaces the entire buffer and places the cursor at the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.buffer = text.into();
        self.cursor = self.buffer.len();
        self.history_pos = None;
    }

    /// Clears the buffer and resets history traversal.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    /// Submits and returns the current buffer, pushing non-empty entries into history.
    pub fn take(&mut self) -> String {
        let taken = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.history_pos = None;
        self.saved_draft.clear();
        let trimmed = taken.trim();
        if !trimmed.is_empty() && self.history.last().map(String::as_str) != Some(trimmed) {
            self.history.push(trimmed.to_owned());
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
        taken
    }

    /// Deletes the character immediately before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_boundary(self.cursor);
        self.buffer.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_pos = None;
    }

    /// Deletes the character at the cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.buffer.len() {
            return;
        }
        let next = self.next_boundary(self.cursor);
        self.buffer.drain(self.cursor..next);
        self.history_pos = None;
    }

    /// Moves the cursor left by one scalar value.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.prev_boundary(self.cursor);
        }
    }

    /// Moves the cursor right by one scalar value.
    pub fn move_right(&mut self) {
        if self.cursor < self.buffer.len() {
            self.cursor = self.next_boundary(self.cursor);
        }
    }

    /// Moves the cursor to the start of the buffer.
    pub const fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the end of the buffer.
    pub const fn move_end(&mut self) {
        self.cursor = self.buffer.len();
    }

    /// Deletes from the start of the buffer to the cursor (`Ctrl+U`).
    pub fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
            self.history_pos = None;
        }
    }

    /// Deletes from the cursor to the end of the buffer (`Ctrl+K`).
    pub fn kill_to_end(&mut self) {
        if self.cursor < self.buffer.len() {
            self.buffer.truncate(self.cursor);
            self.history_pos = None;
        }
    }

    /// Deletes the previous whitespace-delimited word (`Ctrl+W`).
    pub fn delete_prev_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut idx = self.cursor;
        while idx > 0 {
            let prev = self.prev_boundary(idx);
            let Some(ch) = self.buffer[prev..idx].chars().next() else {
                break;
            };
            if !ch.is_whitespace() {
                break;
            }
            idx = prev;
        }
        while idx > 0 {
            let prev = self.prev_boundary(idx);
            let Some(ch) = self.buffer[prev..idx].chars().next() else {
                break;
            };
            if ch.is_whitespace() {
                break;
            }
            idx = prev;
        }
        self.buffer.drain(idx..self.cursor);
        self.cursor = idx;
        self.history_pos = None;
    }

    /// Steps backward in prompt history.
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_idx = match self.history_pos {
            None => {
                self.saved_draft.clone_from(&self.buffer);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(idx) => idx - 1,
        };
        self.history_pos = Some(next_idx);
        self.buffer.clone_from(&self.history[next_idx]);
        self.cursor = self.buffer.len();
    }

    /// Steps forward in prompt history, restoring the draft at the end.
    pub fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            let next_idx = pos + 1;
            self.history_pos = Some(next_idx);
            self.buffer.clone_from(&self.history[next_idx]);
        } else {
            self.history_pos = None;
            self.buffer = std::mem::take(&mut self.saved_draft);
        }
        self.cursor = self.buffer.len();
    }

    /// Computes a visible horizontal window `(visible_slice, cursor_offset_cols)`
    /// fitting within `max_cols`.
    #[must_use]
    pub fn visible_window(&self, max_cols: usize) -> (&str, usize) {
        if max_cols == 0 {
            return ("", 0);
        }
        let total_cols = self.buffer.width();
        let cursor_cols = self.cursor_display_col();
        if total_cols < max_cols {
            return (&self.buffer, cursor_cols);
        }
        let target_start = cursor_cols.saturating_sub(max_cols.saturating_sub(2));
        let mut skipped_cols = 0_usize;
        let mut start_byte = 0_usize;
        for (byte_idx, ch) in self.buffer.char_indices() {
            if skipped_cols >= target_start {
                start_byte = byte_idx;
                break;
            }
            skipped_cols += ch.width().unwrap_or(0);
            start_byte = byte_idx + ch.len_utf8();
        }
        let mut end_byte = start_byte;
        let mut shown_cols = 0_usize;
        for (rel_idx, ch) in self.buffer[start_byte..].char_indices() {
            let w = ch.width().unwrap_or(0);
            if shown_cols + w > max_cols {
                break;
            }
            shown_cols += w;
            end_byte = start_byte + rel_idx + ch.len_utf8();
        }
        (
            &self.buffer[start_byte..end_byte],
            cursor_cols.saturating_sub(skipped_cols),
        )
    }

    fn prev_boundary(&self, from: usize) -> usize {
        self.buffer[..from]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx)
    }

    fn next_boundary(&self, from: usize) -> usize {
        self.buffer[from..]
            .chars()
            .next()
            .map_or(self.buffer.len(), |ch| from + ch.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_cjk_editing_and_display_width() {
        let mut composer = Composer::default();
        composer.insert_str("中文日本語한국어");
        assert_eq!(composer.cursor_display_col(), 16);
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.text(), "中文日本語한어");
        assert_eq!(composer.cursor_display_col(), 12);
    }

    #[test]
    fn history_roundtrips_draft() {
        let mut composer = Composer::default();
        composer.set_text("/sessions");
        assert_eq!(composer.take(), "/sessions");
        composer.set_text("draft");
        composer.history_prev();
        assert_eq!(composer.text(), "/sessions");
        composer.history_next();
        assert_eq!(composer.text(), "draft");
    }
}

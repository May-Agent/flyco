//! Keystroke decoder and single-line editor for the interactive CLI.
//!
//! Pure state machines with zero I/O: raw terminal bytes decode into [`Key`]
//! events (handling multi-byte UTF-8 and CSI/SS3 escape sequences split across
//! read boundaries), and [`LineEditor`] maintains the editable draft, cursor,
//! and input history.

/// Maximum length of an incomplete escape sequence before it is abandoned.
const MAX_ESCAPE_LEN: usize = 16;

/// Maximum number of submitted lines retained in local input history.
const MAX_HISTORY: usize = 200;

/// One logical key press decoded from raw terminal bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    /// A printable Unicode character.
    Char(char),
    /// Return / Enter (`\r` or `\n`).
    Enter,
    /// Backspace (`0x7F` or `0x08`).
    Backspace,
    /// Forward delete (`ESC [ 3 ~`).
    Delete,
    /// Left arrow (`ESC [ D` or `ESC O D`).
    Left,
    /// Right arrow (`ESC [ C` or `ESC O C`).
    Right,
    /// Up arrow (`ESC [ A` or `ESC O A`).
    Up,
    /// Down arrow (`ESC [ B` or `ESC O B`).
    Down,
    /// Start of line (`Ctrl-A`, `Home`).
    Home,
    /// End of line (`Ctrl-E`, `End`).
    End,
    /// Interrupt (`Ctrl-C`, `0x03`).
    CtrlC,
    /// EOF / detach when line is empty (`Ctrl-D`, `0x04`).
    CtrlD,
    /// Explicit detach (`Ctrl-]`, `0x1D`).
    CtrlDetach,
    /// Kill from cursor to start of line (`Ctrl-U`, `0x15`).
    CtrlU,
    /// Kill from cursor to end of line (`Ctrl-K`, `0x0B`).
    CtrlK,
    /// Delete previous word (`Ctrl-W`, `0x17`).
    CtrlW,
    /// Standalone Escape (`0x1B`).
    Escape,
}

/// Incremental decoder turning raw terminal read chunks into [`Key`] values.
#[derive(Debug, Default)]
pub(crate) struct KeyDecoder {
    pending: Vec<u8>,
}

impl KeyDecoder {
    /// Feeds raw bytes from stdin and returns all complete keys decoded.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<Key> {
        let mut buf = core::mem::take(&mut self.pending);
        buf.extend_from_slice(chunk);
        let mut keys = Vec::new();
        let mut i = 0;
        while i < buf.len() {
            match buf[i] {
                0x03 => {
                    keys.push(Key::CtrlC);
                    i += 1;
                }
                0x04 => {
                    keys.push(Key::CtrlD);
                    i += 1;
                }
                0x1D => {
                    keys.push(Key::CtrlDetach);
                    i += 1;
                }
                0x01 => {
                    keys.push(Key::Home);
                    i += 1;
                }
                0x05 => {
                    keys.push(Key::End);
                    i += 1;
                }
                0x15 => {
                    keys.push(Key::CtrlU);
                    i += 1;
                }
                0x0B => {
                    keys.push(Key::CtrlK);
                    i += 1;
                }
                0x17 => {
                    keys.push(Key::CtrlW);
                    i += 1;
                }
                0x0D | 0x0A => {
                    keys.push(Key::Enter);
                    // Swallow `\n` immediately following `\r` (CRLF).
                    if buf[i] == 0x0D && buf.get(i + 1) == Some(&0x0A) {
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                0x7F | 0x08 => {
                    keys.push(Key::Backspace);
                    i += 1;
                }
                0x1B => match parse_escape(&buf[i..]) {
                    EscapeParse::Key(key, consumed) => {
                        keys.push(key);
                        i += consumed;
                    }
                    EscapeParse::Ignore(consumed) => {
                        i += consumed;
                    }
                    EscapeParse::Incomplete => {
                        self.pending.extend_from_slice(&buf[i..]);
                        break;
                    }
                },
                byte if byte < 0x20 => {
                    // Unhandled control byte.
                    i += 1;
                }
                _ => match decode_utf8_char(&buf[i..]) {
                    Utf8Char::Char(ch, len) => {
                        keys.push(Key::Char(ch));
                        i += len;
                    }
                    Utf8Char::Incomplete => {
                        self.pending.extend_from_slice(&buf[i..]);
                        break;
                    }
                    Utf8Char::Invalid(len) => {
                        i += len;
                    }
                },
            }
        }
        keys
    }
}

enum EscapeParse {
    Key(Key, usize),
    Ignore(usize),
    Incomplete,
}

fn parse_escape(slice: &[u8]) -> EscapeParse {
    debug_assert_eq!(slice.first(), Some(&0x1B));
    if slice.len() == 1 {
        return EscapeParse::Key(Key::Escape, 1);
    }
    match slice[1] {
        b'[' => parse_csi(slice),
        b'O' => {
            if slice.len() < 3 {
                return EscapeParse::Incomplete;
            }
            let key = match slice[2] {
                b'A' => Some(Key::Up),
                b'B' => Some(Key::Down),
                b'C' => Some(Key::Right),
                b'D' => Some(Key::Left),
                b'H' => Some(Key::Home),
                b'F' => Some(Key::End),
                _ => None,
            };
            key.map_or(EscapeParse::Ignore(3), |k| EscapeParse::Key(k, 3))
        }
        _ => EscapeParse::Ignore(2),
    }
}

fn parse_csi(slice: &[u8]) -> EscapeParse {
    // CSI starts with ESC `[` followed by parameter bytes `0x30..=0x3F`,
    // intermediate bytes `0x20..=0x2F`, and ends with a final byte `0x40..=0x7E`.
    let limit = slice.len().min(MAX_ESCAPE_LEN);
    for end in 2..limit {
        let byte = slice[end];
        if (0x40..=0x7E).contains(&byte) {
            let params = &slice[2..end];
            let consumed = end + 1;
            let key = match (byte, params) {
                (b'A', _) => Some(Key::Up),
                (b'B', _) => Some(Key::Down),
                (b'C', _) => Some(Key::Right),
                (b'D', _) => Some(Key::Left),
                (b'H', _) | (b'~', b"1" | b"7") => Some(Key::Home),
                (b'F', _) | (b'~', b"4" | b"8") => Some(Key::End),
                (b'~', b"3") => Some(Key::Delete),
                _ => None,
            };
            return key.map_or(EscapeParse::Ignore(consumed), |k| {
                EscapeParse::Key(k, consumed)
            });
        }
    }
    if slice.len() >= MAX_ESCAPE_LEN {
        EscapeParse::Ignore(slice.len())
    } else {
        EscapeParse::Incomplete
    }
}

enum Utf8Char {
    Char(char, usize),
    Incomplete,
    Invalid(usize),
}

fn decode_utf8_char(slice: &[u8]) -> Utf8Char {
    match core::str::from_utf8(slice) {
        Ok(text) => text
            .chars()
            .next()
            .map_or(Utf8Char::Incomplete, |ch| Utf8Char::Char(ch, ch.len_utf8())),
        Err(error) => {
            if error.valid_up_to() > 0 {
                let valid =
                    core::str::from_utf8(&slice[..error.valid_up_to()]).expect("valid UTF-8");
                let ch = valid.chars().next().expect("non-empty valid UTF-8");
                Utf8Char::Char(ch, ch.len_utf8())
            } else if let Some(invalid_len) = error.error_len() {
                Utf8Char::Invalid(invalid_len.max(1))
            } else {
                Utf8Char::Incomplete
            }
        }
    }
}

/// Editable single-line prompt buffer with cursor movement and history.
#[derive(Debug, Default)]
pub(crate) struct LineEditor {
    buf: String,
    /// Byte offset of the cursor; always lies on a UTF-8 char boundary.
    cursor: usize,
    history: Vec<String>,
    history_pos: Option<usize>,
    saved_draft: String,
}

impl LineEditor {
    /// Current draft text.
    #[must_use]
    pub(crate) fn text(&self) -> &str {
        &self.buf
    }

    /// Whether the draft is empty.
    #[must_use]
    pub(crate) const fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Number of Unicode scalar values after the cursor (used to step back
    /// from the end of the rendered line when redrawing).
    #[must_use]
    pub(crate) fn chars_after_cursor(&self) -> usize {
        self.buf[self.cursor..].chars().count()
    }

    /// Inserts a character at the cursor.
    pub(crate) fn insert_char(&mut self, ch: char) {
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.history_pos = None;
    }

    /// Deletes the character immediately before the cursor.
    pub(crate) fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prev_char_boundary();
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
        self.history_pos = None;
    }

    /// Deletes the character under the cursor.
    pub(crate) fn delete(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let next = self.next_char_boundary();
        self.buf.drain(self.cursor..next);
        self.history_pos = None;
    }

    /// Moves the cursor one character left.
    pub(crate) fn move_left(&mut self) {
        self.cursor = self.prev_char_boundary();
    }

    /// Moves the cursor one character right.
    pub(crate) fn move_right(&mut self) {
        self.cursor = self.next_char_boundary();
    }

    /// Moves the cursor to the start of the line.
    pub(crate) const fn move_home(&mut self) {
        self.cursor = 0;
    }

    /// Moves the cursor to the end of the line.
    pub(crate) const fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    /// Kills text from the start of the line up to the cursor.
    pub(crate) fn kill_to_start(&mut self) {
        if self.cursor > 0 {
            self.buf.drain(..self.cursor);
            self.cursor = 0;
            self.history_pos = None;
        }
    }

    /// Kills text from the cursor to the end of the line.
    pub(crate) fn kill_to_end(&mut self) {
        if self.cursor < self.buf.len() {
            self.buf.truncate(self.cursor);
            self.history_pos = None;
        }
    }

    /// Deletes the word immediately preceding the cursor.
    pub(crate) fn delete_prev_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let before = &self.buf[..self.cursor];
        let trimmed_end = before.trim_end().len();
        let start = before[..trimmed_end]
            .rfind(char::is_whitespace)
            .map_or(0, |idx| {
                idx + before[idx..].chars().next().map_or(0, char::len_utf8)
            });
        self.buf.drain(start..self.cursor);
        self.cursor = start;
        self.history_pos = None;
    }

    /// Steps backward in local input history.
    pub(crate) fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next_pos = match self.history_pos {
            None => {
                self.saved_draft.clone_from(&self.buf);
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(pos) => pos - 1,
        };
        self.history_pos = Some(next_pos);
        self.buf.clone_from(&self.history[next_pos]);
        self.cursor = self.buf.len();
    }

    /// Steps forward in local input history, restoring the saved draft at the end.
    pub(crate) fn history_next(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            let next_pos = pos + 1;
            self.history_pos = Some(next_pos);
            self.buf.clone_from(&self.history[next_pos]);
        } else {
            self.history_pos = None;
            self.buf = core::mem::take(&mut self.saved_draft);
        }
        self.cursor = self.buf.len();
    }

    /// Replaces the draft with `text`, placing the cursor at the end.
    pub(crate) fn set_text(&mut self, text: String) {
        self.buf = text;
        self.cursor = self.buf.len();
        self.history_pos = None;
    }

    /// Clears the current draft.
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.history_pos = None;
    }

    /// Records a line in local history (skipping empties and consecutive dupes).
    pub(crate) fn push_history(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.history.last().is_some_and(|last| last == &line) {
            return;
        }
        self.history.push(line);
        if self.history.len() > MAX_HISTORY {
            self.history.remove(0);
        }
    }

    /// Takes the current draft, recording non-empty lines in history.
    pub(crate) fn take_line(&mut self) -> String {
        let line = core::mem::take(&mut self.buf);
        self.cursor = 0;
        self.history_pos = None;
        self.saved_draft.clear();
        self.push_history(line.clone());
        line
    }

    fn prev_char_boundary(&self) -> usize {
        self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(idx, _)| idx)
    }

    fn next_char_boundary(&self) -> usize {
        self.buf[self.cursor..]
            .chars()
            .next()
            .map_or(self.buf.len(), |ch| self.cursor + ch.len_utf8())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_split_utf8_cjk_and_csi_sequences() {
        let mut decoder = KeyDecoder::default();
        // Split Chinese "中" (E4 B8 AD), Japanese "あ" (E3 81 82), Korean "한" (ED 95 9C),
        // and CSI arrow sequence ESC [ A across chunks.
        assert_eq!(decoder.push(&[0xE4, 0xB8]), Vec::<Key>::new());
        assert_eq!(decoder.push(&[0xAD]), vec![Key::Char('中')]);
        assert_eq!(
            decoder.push("あ한".as_bytes()),
            vec![Key::Char('あ'), Key::Char('한')]
        );

        assert_eq!(decoder.push(b"\x1b["), Vec::<Key>::new());
        assert_eq!(decoder.push(b"A\x1b[3~"), vec![Key::Up, Key::Delete]);
    }

    #[test]
    fn decoder_maps_control_keys_and_crlf() {
        let mut decoder = KeyDecoder::default();
        let keys = decoder.push(b"\x03\x04\x1d\r\n\x7f");
        assert_eq!(
            keys,
            vec![
                Key::CtrlC,
                Key::CtrlD,
                Key::CtrlDetach,
                Key::Enter,
                Key::Backspace,
            ]
        );
    }

    #[test]
    fn editor_edits_utf8_and_navigates_history() {
        let mut ed = LineEditor::default();
        for ch in "hello 世界".chars() {
            ed.insert_char(ch);
        }
        ed.move_left();
        ed.insert_char('!');
        assert_eq!(ed.text(), "hello 世!界");
        assert_eq!(ed.chars_after_cursor(), 1);
        ed.backspace();
        assert_eq!(ed.text(), "hello 世界");

        let first = ed.take_line();
        assert_eq!(first, "hello 世界");
        assert!(ed.is_empty());

        ed.set_text("second".to_owned());
        let _ = ed.take_line();

        ed.set_text("wip".to_owned());
        ed.history_prev();
        assert_eq!(ed.text(), "second");
        ed.history_prev();
        assert_eq!(ed.text(), "hello 世界");
        ed.history_next();
        assert_eq!(ed.text(), "second");
        ed.history_next();
        assert_eq!(ed.text(), "wip");
    }

    #[test]
    fn editor_word_and_line_kill() {
        let mut ed = LineEditor::default();
        ed.set_text("cargo nextest run".to_owned());
        ed.delete_prev_word();
        assert_eq!(ed.text(), "cargo nextest ");
        ed.move_home();
        ed.move_right();
        ed.kill_to_end();
        assert_eq!(ed.text(), "c");
        ed.kill_to_start();
        assert!(ed.is_empty());
    }
}

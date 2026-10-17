//! Single-line text editing shared by the inline rename field and the toolbar
//! search field. Text is kept as chars so the caret can't split a UTF-8 sequence.

const BACK: u16 = 0x08;
const END: u16 = 0x23;
const HOME: u16 = 0x24;
const LEFT: u16 = 0x25;
const UP: u16 = 0x26;
const RIGHT: u16 = 0x27;
const DOWN: u16 = 0x28;
const DEL: u16 = 0x2E;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyResult {
    /// Not an editing key; the caller may treat it as a shortcut.
    Ignored,
    /// Caret or selection moved.
    Moved,
    /// Text changed.
    Edited,
}

#[derive(Clone, Default, Debug)]
pub struct TextField {
    pub text: Vec<char>,
    pub caret: usize,
    pub anchor: usize,
}

impl TextField {
    pub fn new(s: &str) -> Self {
        let text: Vec<char> = s.chars().collect();
        let n = text.len();
        Self { text, caret: n, anchor: n }
    }

    pub fn range(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn string(&self) -> String {
        self.text.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Moves the caret to `i`, extending the selection from the anchor if `extend`.
    pub fn set_caret(&mut self, i: usize, extend: bool) {
        self.caret = i.min(self.text.len());
        if !extend {
            self.anchor = self.caret;
        }
    }

    pub fn selected(&self) -> String {
        let (a, b) = self.range();
        self.text[a..b].iter().collect()
    }

    /// Replaces the selection with `s` (line breaks become spaces).
    pub fn insert(&mut self, s: &str) {
        let (a, b) = self.range();
        let chars: Vec<char> = s.chars().map(|c| if c == '\r' || c == '\n' || c == '\t' { ' ' } else { c }).collect();
        let n = chars.len();
        self.text.splice(a..b, chars);
        self.caret = a + n;
        self.anchor = self.caret;
    }

    fn word_left(&self, mut i: usize) -> usize {
        while i > 0 && !self.text[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && self.text[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let n = self.text.len();
        while i < n && !self.text[i].is_alphanumeric() {
            i += 1;
        }
        while i < n && self.text[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Editing keys (Ctrl stands in for ⌥ word moves and ⌘ shortcuts).
    pub fn on_key(&mut self, vk: u16, ctrl: bool, shift: bool) -> KeyResult {
        let len = self.text.len();
        let (a, b) = self.range();
        match vk {
            BACK | DEL => {
                if a != b {
                    self.insert("");
                } else if vk == BACK && a > 0 {
                    let from = if ctrl { self.word_left(a) } else { a - 1 };
                    self.text.drain(from..a);
                    self.set_caret(from, false);
                } else if vk == DEL && a < len {
                    let to = if ctrl { self.word_right(a) } else { a + 1 };
                    self.text.drain(a..to);
                    self.set_caret(a, false);
                } else {
                    return KeyResult::Moved;
                }
                KeyResult::Edited
            }
            LEFT | RIGHT | HOME | END | UP | DOWN => {
                let to = match vk {
                    LEFT if a != b && !shift => a,
                    RIGHT if a != b && !shift => b,
                    LEFT if ctrl => self.word_left(self.caret),
                    RIGHT if ctrl => self.word_right(self.caret),
                    LEFT => self.caret.saturating_sub(1),
                    RIGHT => (self.caret + 1).min(len),
                    HOME | UP => 0,
                    _ => len,
                };
                self.set_caret(to, shift);
                KeyResult::Moved
            }
            _ if ctrl && vk == b'A' as u16 => {
                self.select_all();
                KeyResult::Moved
            }
            _ if ctrl && (vk == b'C' as u16 || vk == b'X' as u16) => {
                if a != b {
                    super::fs::copy_text(&self.selected());
                    if vk == b'X' as u16 {
                        self.insert("");
                        return KeyResult::Edited;
                    }
                }
                KeyResult::Moved
            }
            _ if ctrl && vk == b'V' as u16 => match super::fs::paste_text() {
                Some(s) => {
                    self.insert(&s);
                    KeyResult::Edited
                }
                None => KeyResult::Moved,
            },
            _ => KeyResult::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_selection() {
        let mut f = TextField::new("hello world");
        f.anchor = 0;
        f.caret = 5;
        f.insert("bye");
        assert_eq!(f.string(), "bye world");
        assert_eq!((f.caret, f.anchor), (3, 3));
        f.insert("\r\nx");
        assert_eq!(f.string(), "bye  x world");
    }

    #[test]
    fn deletes_chars_and_words() {
        let mut f = TextField::new("foo bar.txt");
        assert_eq!(f.on_key(BACK, false, false), KeyResult::Edited);
        assert_eq!(f.string(), "foo bar.tx");
        assert_eq!(f.on_key(BACK, true, false), KeyResult::Edited);
        assert_eq!(f.string(), "foo bar.");
        f.set_caret(0, false);
        assert_eq!(f.on_key(DEL, true, false), KeyResult::Edited);
        assert_eq!(f.string(), " bar.");
        assert_eq!(f.on_key(BACK, false, false), KeyResult::Moved);
    }

    #[test]
    fn caret_moves_and_extends() {
        let mut f = TextField::new("ab cd");
        f.on_key(LEFT, true, false);
        assert_eq!(f.caret, 3);
        f.on_key(HOME, false, true);
        assert_eq!(f.range(), (0, 3));
        assert_eq!(f.selected(), "ab ");
        // Left without shift collapses to the selection start.
        f.on_key(RIGHT, false, false);
        assert_eq!((f.caret, f.anchor), (3, 3));
        f.on_key(b'A' as u16, true, false);
        assert_eq!(f.range(), (0, 5));
        assert_eq!(f.on_key(b'Q' as u16, true, false), KeyResult::Ignored);
    }

    #[test]
    fn multibyte_text_is_safe() {
        let mut f = TextField::new("naïve 日本");
        f.on_key(BACK, false, false);
        assert_eq!(f.string(), "naïve 日");
        f.set_caret(3, false);
        f.on_key(BACK, false, false);
        assert_eq!(f.string(), "nave 日");
    }
}

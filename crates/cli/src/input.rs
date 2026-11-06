//! A single-line editor with vi modes, matching yazi's input layer.
//!
//! Positions are **character** indices (never byte offsets), so multi-byte
//! names edit naturally; widths are measured in terminal cells when deciding
//! what fits in the box.

use unicode_width::UnicodeWidthChar;

use crate::keymap::Cmd;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    Replace,
}

#[derive(Clone, Debug)]
struct Snap {
    text: Vec<char>,
    cursor: usize,
}

/// What a visible slice of the line looks like.
pub struct View {
    pub text: String,
    pub cursor_col: usize,
    /// Selected range within `text`, in cells.
    pub selection: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct Input {
    text: Vec<char>,
    cursor: usize,
    pub mode: Mode,
    anchor: Option<usize>,
    offset: usize,
    undo: Vec<Snap>,
    redo: Vec<Snap>,
    register: String,
    /// When set, the text is obscured (not used for files, kept for parity).
    pub obscure: bool,
}

impl Default for Input {
    fn default() -> Self {
        Self::new("", Mode::Insert)
    }
}

/// Character classes for word motions.
fn class(c: char, far: bool) -> u8 {
    if c.is_whitespace() {
        0
    } else if far || c.is_alphanumeric() || c == '_' {
        1
    } else {
        2
    }
}

impl Input {
    pub fn new(text: &str, mode: Mode) -> Self {
        let text: Vec<char> = text.chars().collect();
        let cursor = text.len();
        Self {
            text,
            cursor,
            mode,
            anchor: None,
            offset: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            register: String::new(),
            obscure: false,
        }
    }

    pub fn value(&self) -> String {
        self.text.iter().collect()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn set_cursor(&mut self, c: usize) {
        self.cursor = c.min(self.text.len());
        self.clamp();
    }

    pub fn set_value(&mut self, s: &str) {
        self.snapshot();
        self.text = s.chars().collect();
        self.cursor = self.text.len();
        self.clamp();
    }

    fn snapshot(&mut self) {
        self.undo.push(Snap {
            text: self.text.clone(),
            cursor: self.cursor,
        });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// In normal/visual mode the cursor sits *on* a character, so it can't be
    /// past the last one.
    fn clamp(&mut self) {
        let max = if self.mode == Mode::Insert {
            self.text.len()
        } else {
            self.text.len().saturating_sub(1)
        };
        self.cursor = self.cursor.min(max);
    }

    pub fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        let (s, e) = if a <= self.cursor {
            (a, self.cursor)
        } else {
            (self.cursor, a)
        };
        Some((s, (e + 1).min(self.text.len())))
    }

    pub fn type_char(&mut self, c: char) {
        match self.mode {
            Mode::Insert => {
                self.snapshot();
                self.text.insert(self.cursor, c);
                self.cursor += 1;
            }
            Mode::Replace => {
                self.snapshot();
                if self.cursor < self.text.len() {
                    self.text[self.cursor] = c;
                }
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    pub fn type_str(&mut self, s: &str) {
        if self.mode != Mode::Insert {
            return;
        }
        self.snapshot();
        for c in s.chars().filter(|c| !c.is_control()) {
            self.text.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn word_back(&self, far: bool) -> usize {
        let mut i = self.cursor;
        while i > 0 && class(self.text[i - 1], far) == 0 {
            i -= 1;
        }
        if i == 0 {
            return 0;
        }
        let k = class(self.text[i - 1], far);
        while i > 0 && class(self.text[i - 1], far) == k {
            i -= 1;
        }
        i
    }

    fn word_forward(&self, far: bool) -> usize {
        let n = self.text.len();
        let mut i = self.cursor;
        if i >= n {
            return n;
        }
        let k = class(self.text[i], far);
        while i < n && class(self.text[i], far) == k && k != 0 {
            i += 1;
        }
        while i < n && class(self.text[i], far) == 0 {
            i += 1;
        }
        i
    }

    fn word_end(&self, far: bool) -> usize {
        let n = self.text.len();
        if n == 0 {
            return 0;
        }
        let mut i = self.cursor + 1;
        while i < n && class(self.text[i], far) == 0 {
            i += 1;
        }
        if i >= n {
            return n - 1;
        }
        let k = class(self.text[i], far);
        while i + 1 < n && class(self.text[i + 1], far) == k {
            i += 1;
        }
        i
    }

    fn delete_range(&mut self, s: usize, e: usize, cut: bool) {
        if s >= e {
            return;
        }
        self.snapshot();
        let removed: String = self.text.drain(s..e).collect();
        if cut {
            self.register = removed;
        }
        self.cursor = s;
    }

    /// Run an input-layer command. Returns `Some(submit)` when the input
    /// should close (`true` = submit, `false` = cancel).
    pub fn exec(&mut self, cmd: &Cmd) -> Option<bool> {
        let (_, name) = cmd.layer();
        match name {
            "close" => return Some(cmd.flag("submit")),
            "escape" => match self.mode {
                Mode::Insert => {
                    self.mode = Mode::Normal;
                    self.cursor = self.cursor.saturating_sub(1);
                    self.clamp();
                }
                Mode::Visual | Mode::Replace => {
                    self.mode = Mode::Normal;
                    self.anchor = None;
                    self.clamp();
                }
                Mode::Normal => return Some(false),
            },
            "insert" => {
                if self.mode == Mode::Visual {
                    self.anchor = None;
                }
                let append = cmd.flag("append");
                self.mode = Mode::Insert;
                if append && !self.text.is_empty() {
                    self.cursor = (self.cursor + 1).min(self.text.len());
                }
            }
            "visual" => {
                self.mode = Mode::Visual;
                self.anchor = Some(self.cursor);
            }
            "replace" => {
                if !self.text.is_empty() {
                    self.mode = Mode::Replace;
                }
            }
            "move" => {
                let arg = cmd.arg(0).unwrap_or("0");
                self.cursor = match arg {
                    "bol" => 0,
                    "eol" => self.text.len(),
                    "first-char" => self
                        .text
                        .iter()
                        .position(|c| !c.is_whitespace())
                        .unwrap_or(0),
                    n => {
                        let d: isize = n.parse().unwrap_or(0);
                        (self.cursor as isize + d).max(0) as usize
                    }
                };
                self.clamp();
            }
            "backward" => {
                self.cursor = self.word_back(cmd.flag("far"));
                self.clamp();
            }
            "forward" => {
                let far = cmd.flag("far");
                self.cursor = if cmd.flag("end-of-word") {
                    let e = self.word_end(far);
                    if self.mode == Mode::Insert {
                        (e + 1).min(self.text.len())
                    } else {
                        e
                    }
                } else {
                    self.word_forward(far)
                };
                self.clamp();
            }
            "backspace" => {
                if cmd.flag("under") {
                    if self.cursor < self.text.len() {
                        self.delete_range(self.cursor, self.cursor + 1, false);
                    }
                } else if self.cursor > 0 {
                    self.delete_range(self.cursor - 1, self.cursor, false);
                }
                self.clamp();
            }
            "kill" => {
                let (s, e) = match cmd.arg(0).unwrap_or("") {
                    "bol" => (0, self.cursor),
                    "eol" => (self.cursor, self.text.len()),
                    "backward" => (self.word_back(false), self.cursor),
                    "forward" => (self.cursor, self.word_forward(false)),
                    _ => (self.cursor, self.cursor),
                };
                self.delete_range(s, e, true);
                self.clamp();
            }
            "delete" => {
                let (s, e) = match self.selection() {
                    Some(r) => r,
                    None if self.cursor < self.text.len() => (self.cursor, self.cursor + 1),
                    None => (self.cursor, self.cursor),
                };
                self.delete_range(s, e, cmd.flag("cut"));
                self.anchor = None;
                self.mode = if cmd.flag("insert") {
                    Mode::Insert
                } else {
                    Mode::Normal
                };
                self.clamp();
            }
            "yank" => {
                if let Some((s, e)) = self.selection() {
                    self.register = self.text[s..e].iter().collect();
                    self.cursor = s;
                }
                self.anchor = None;
                self.mode = Mode::Normal;
                self.clamp();
            }
            "paste" => {
                if self.register.is_empty() {
                    return None;
                }
                self.snapshot();
                let at = if cmd.flag("before") || self.text.is_empty() {
                    self.cursor
                } else {
                    (self.cursor + 1).min(self.text.len())
                };
                let reg: Vec<char> = self.register.chars().collect();
                let n = reg.len();
                self.text.splice(at..at, reg);
                self.cursor = at + n.saturating_sub(1);
                self.clamp();
            }
            "undo" => {
                if self.mode == Mode::Visual {
                    return None;
                }
                if let Some(s) = self.undo.pop() {
                    self.redo.push(Snap {
                        text: std::mem::replace(&mut self.text, s.text),
                        cursor: self.cursor,
                    });
                    self.cursor = s.cursor;
                    self.clamp();
                }
            }
            "redo" => {
                if let Some(s) = self.redo.pop() {
                    self.undo.push(Snap {
                        text: std::mem::replace(&mut self.text, s.text),
                        cursor: self.cursor,
                    });
                    self.cursor = s.cursor;
                    self.clamp();
                }
            }
            "casefy" => {
                if let Some((s, e)) = self.selection() {
                    self.snapshot();
                    let upper = cmd.arg(0) == Some("upper");
                    for c in &mut self.text[s..e] {
                        *c = if upper {
                            c.to_uppercase().next().unwrap_or(*c)
                        } else {
                            c.to_lowercase().next().unwrap_or(*c)
                        };
                    }
                    self.anchor = None;
                    self.mode = Mode::Normal;
                    self.cursor = s;
                }
            }
            _ => {}
        }
        None
    }

    /// The part of the line that fits in `width` cells, scrolled to keep the
    /// cursor visible.
    pub fn view(&mut self, width: usize) -> View {
        let width = width.max(1);
        let w = |c: &char| c.width().unwrap_or(0);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        loop {
            let used: usize = self.text[self.offset..self.cursor].iter().map(w).sum();
            if used < width || self.offset >= self.cursor {
                break;
            }
            self.offset += 1;
        }
        let mut text = String::new();
        let mut cells = 0;
        let mut cursor_col = 0;
        let sel = self.selection();
        let mut sel_cells: Option<(usize, usize)> = None;
        for (i, c) in self.text.iter().enumerate().skip(self.offset) {
            let cw = w(c);
            if cells + cw > width {
                break;
            }
            if i == self.cursor {
                cursor_col = cells;
            }
            if let Some((s, e)) = sel {
                if i >= s && i < e {
                    let r = sel_cells.get_or_insert((cells, cells));
                    r.1 = cells + cw;
                }
            }
            text.push(if self.obscure { '•' } else { *c });
            cells += cw;
        }
        if self.cursor >= self.text.len() {
            cursor_col = cells;
        }
        View {
            text,
            cursor_col,
            selection: sel_cells,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(i: &mut Input, s: &str) -> Option<bool> {
        i.exec(&Cmd::parse(s).unwrap())
    }

    #[test]
    fn insert_and_backspace() {
        let mut i = Input::new("ab", Mode::Insert);
        i.type_char('c');
        assert_eq!(i.value(), "abc");
        run(&mut i, "backspace");
        assert_eq!(i.value(), "ab");
        run(&mut i, "move bol");
        run(&mut i, "backspace --under");
        assert_eq!(i.value(), "b");
    }

    #[test]
    fn word_motions_and_kill() {
        let mut i = Input::new("foo bar.baz", Mode::Insert);
        run(&mut i, "kill backward");
        assert_eq!(i.value(), "foo bar.");
        run(&mut i, "backward");
        assert_eq!(i.cursor(), 7);
        run(&mut i, "kill bol");
        assert_eq!(i.value(), ".");
    }

    #[test]
    fn escape_then_escape_cancels() {
        let mut i = Input::new("x", Mode::Insert);
        assert_eq!(run(&mut i, "escape"), None);
        assert_eq!(i.mode, Mode::Normal);
        assert_eq!(run(&mut i, "escape"), Some(false));
        assert_eq!(run(&mut i, "close --submit"), Some(true));
    }

    #[test]
    fn visual_yank_paste_undo() {
        let mut i = Input::new("hello", Mode::Normal);
        run(&mut i, "move bol");
        run(&mut i, "visual");
        run(&mut i, "move 1");
        run(&mut i, "yank");
        run(&mut i, "move eol");
        run(&mut i, "paste");
        assert_eq!(i.value(), "hellohe");
        run(&mut i, "undo");
        assert_eq!(i.value(), "hello");
        run(&mut i, "redo");
        assert_eq!(i.value(), "hellohe");
    }

    #[test]
    fn view_scrolls() {
        let mut i = Input::new("abcdefghij", Mode::Insert);
        let v = i.view(5);
        assert_eq!(v.cursor_col, 4);
        assert!(v.text.ends_with('j'));
    }

    #[test]
    fn multibyte() {
        let mut i = Input::new("añb", Mode::Insert);
        run(&mut i, "backspace");
        run(&mut i, "backspace");
        assert_eq!(i.value(), "a");
    }
}

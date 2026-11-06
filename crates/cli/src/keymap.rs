//! Keymap in yazi's format: `{ on = "<C-a>" | ["g", "g"], run = "cmd args" | [...], desc }`.
//!
//! Bindings are grouped by layer (`mgr`, `tasks`, `spot`, `pick`, `input`,
//! `confirm`, `cmp`, `help`). Within a layer the first match wins, so a user's
//! `prepend_keymap` overrides the preset.

use std::collections::HashMap;
use std::fmt;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::config::{load_layered, PRESET_KEYMAP};
use crate::util::split_words;

/// A normalized key press.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn plain(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    /// Normalize a terminal key event. For characters, Shift is folded into
    /// the character itself (`G`, `?`), so `"G"` matches regardless of how the
    /// terminal reports it.
    pub fn from_event(ev: KeyEvent) -> Self {
        let m = ev.modifiers;
        let mut code = ev.code;
        let mut shift = m.contains(KeyModifiers::SHIFT);
        if let KeyCode::Char(c) = code {
            if shift && c.is_ascii_lowercase() {
                code = KeyCode::Char(c.to_ascii_uppercase());
            }
            shift = false;
        }
        if code == KeyCode::BackTab {
            code = KeyCode::Tab;
            shift = true;
        }
        Self {
            code,
            ctrl: m.contains(KeyModifiers::CONTROL),
            alt: m.contains(KeyModifiers::ALT),
            shift,
        }
    }

    /// A printable character with no Ctrl/Alt, i.e. something to type.
    pub fn typed_char(&self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }

    /// Parse yazi notation: `a`, `<C-a>`, `<S-Enter>`, `<A-b>`, `<Space>`, `<F1>`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut k = Self::plain(KeyCode::Null);
        let inner = if s.len() > 2 && s.starts_with('<') && s.ends_with('>') {
            &s[1..s.len() - 1]
        } else {
            let mut chars = s.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            k.code = KeyCode::Char(c);
            return Some(k);
        };
        let mut rest = inner;
        loop {
            if rest.len() > 2 && rest.as_bytes()[1] == b'-' {
                match &rest[..1] {
                    "C" | "c" => k.ctrl = true,
                    "A" | "a" | "M" | "m" => k.alt = true,
                    "S" | "s" => k.shift = true,
                    "D" | "d" => {} // Super: not distinguishable in most terminals
                    _ => break,
                }
                rest = &rest[2..];
            } else {
                break;
            }
        }
        k.code = match rest.to_ascii_lowercase().as_str() {
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "enter" => KeyCode::Enter,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "tab" => KeyCode::Tab,
            "backtab" => {
                k.shift = true;
                KeyCode::Tab
            }
            "delete" => KeyCode::Delete,
            "insert" => KeyCode::Insert,
            "esc" => KeyCode::Esc,
            "lt" => KeyCode::Char('<'),
            "gt" => KeyCode::Char('>'),
            f if f.len() > 1 && f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
                KeyCode::F(f[1..].parse().unwrap())
            }
            _ => {
                let mut chars = rest.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if k.shift && c.is_ascii_alphabetic() {
                    k.shift = false;
                    KeyCode::Char(c.to_ascii_uppercase())
                } else if k.ctrl && c.is_ascii_uppercase() {
                    // `<C-A>` is the same chord as `<C-a>` in a terminal.
                    KeyCode::Char(c.to_ascii_lowercase())
                } else {
                    KeyCode::Char(c)
                }
            }
        };
        if let KeyCode::Char(_) = k.code {
            k.shift = false;
        }
        Some(k)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::Backspace => "Backspace".into(),
            KeyCode::Enter => "Enter".into(),
            KeyCode::Left => "Left".into(),
            KeyCode::Right => "Right".into(),
            KeyCode::Up => "Up".into(),
            KeyCode::Down => "Down".into(),
            KeyCode::Home => "Home".into(),
            KeyCode::End => "End".into(),
            KeyCode::PageUp => "PageUp".into(),
            KeyCode::PageDown => "PageDown".into(),
            KeyCode::Tab => "Tab".into(),
            KeyCode::Delete => "Delete".into(),
            KeyCode::Insert => "Insert".into(),
            KeyCode::Esc => "Esc".into(),
            KeyCode::F(n) => format!("F{n}"),
            _ => "?".into(),
        };
        let special = !matches!(self.code, KeyCode::Char(c) if c != ' ');
        if !(self.ctrl || self.alt || self.shift || special) {
            return f.write_str(&name);
        }
        f.write_str("<")?;
        if self.ctrl {
            f.write_str("C-")?;
        }
        if self.alt {
            f.write_str("A-")?;
        }
        if self.shift {
            f.write_str("S-")?;
        }
        write!(f, "{name}>")
    }
}

/// One parsed command: `name positional... --flag --key=value`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cmd {
    pub name: String,
    pub args: Vec<String>,
    /// `--key=value` options (a short list; cheaper than a map to build).
    pub named: Vec<(String, String)>,
}

impl Cmd {
    pub fn parse(s: &str) -> Option<Self> {
        let words = split_words(s);
        let mut it = words.into_iter();
        let name = it.next()?;
        let mut cmd = Cmd {
            name,
            ..Default::default()
        };
        let mut only_positional = false;
        for w in it {
            if only_positional {
                cmd.args.push(w);
            } else if w == "--" {
                only_positional = true;
            } else if let Some(flag) = w.strip_prefix("--") {
                match flag.split_once('=') {
                    Some((k, v)) => cmd.named.push((k.to_string(), v.to_string())),
                    None => cmd.named.push((flag.to_string(), "yes".into())),
                };
            } else {
                cmd.args.push(w);
            }
        }
        Some(cmd)
    }

    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    pub fn arg(&self, i: usize) -> Option<&str> {
        self.args.get(i).map(String::as_str)
    }

    /// A boolean flag: `--x`, `--x=yes`, `--x=true`.
    pub fn flag(&self, k: &str) -> bool {
        matches!(self.opt(k), Some("yes" | "true" | "1"))
    }

    pub fn opt(&self, k: &str) -> Option<&str> {
        self.named
            .iter()
            .rev()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    }

    /// Split `layer:name` into its parts.
    pub fn layer(&self) -> (Option<&str>, &str) {
        match self.name.split_once(':') {
            Some((l, n)) => (Some(l), n),
            None => (None, &self.name),
        }
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        for a in &self.args {
            if a.contains(char::is_whitespace) {
                write!(f, " '{a}'")?;
            } else {
                write!(f, " {a}")?;
            }
        }
        for (k, v) in &self.named {
            if v == "yes" {
                write!(f, " --{k}")?;
            } else {
                write!(f, " --{k}={v}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Binding {
    pub on: Vec<Key>,
    pub run: Vec<Cmd>,
    pub desc: String,
}

impl Binding {
    pub fn on_str(&self) -> String {
        self.on
            .iter()
            .map(|k| k.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn run_str(&self) -> String {
        self.run
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Layer {
    Mgr,
    Tasks,
    Spot,
    Pick,
    Input,
    Confirm,
    Cmp,
    Help,
}

impl Layer {
    pub const ALL: [Layer; 8] = [
        Layer::Mgr,
        Layer::Tasks,
        Layer::Spot,
        Layer::Pick,
        Layer::Input,
        Layer::Confirm,
        Layer::Cmp,
        Layer::Help,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Layer::Mgr => "mgr",
            Layer::Tasks => "tasks",
            Layer::Spot => "spot",
            Layer::Pick => "pick",
            Layer::Input => "input",
            Layer::Confirm => "confirm",
            Layer::Cmp => "cmp",
            Layer::Help => "help",
        }
    }
}

/// What a key press resolved to.
pub enum Lookup<'a> {
    /// Run these commands.
    Run(&'a Binding),
    /// A chord prefix: show these candidates and wait for more keys.
    Prefix(Vec<&'a Binding>),
    /// Nothing bound.
    None,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Keymap {
    layers: HashMap<Layer, Vec<Binding>>,
    pub warnings: Vec<String>,
}

impl Keymap {
    pub fn load() -> Self {
        let (t, warning) = load_layered(PRESET_KEYMAP, "keymap.toml");
        let mut km = Self::from_table(&t);
        km.warnings.extend(warning);
        km
    }

    pub fn from_table(t: &Table) -> Self {
        let mut km = Keymap::default();
        for layer in Layer::ALL {
            let mut list = Vec::new();
            let items = t
                .get(layer.name())
                .and_then(|l| l.get("keymap"))
                .and_then(Value::as_array);
            for item in items.into_iter().flatten() {
                let Some(item) = item.as_table() else {
                    continue;
                };
                let on: Option<Vec<Key>> = match item.get("on") {
                    Some(Value::String(s)) => Key::parse(s).map(|k| vec![k]),
                    Some(Value::Array(a)) => {
                        a.iter().map(|v| v.as_str().and_then(Key::parse)).collect()
                    }
                    _ => None,
                };
                let Some(on) = on.filter(|o| !o.is_empty()) else {
                    km.warnings
                        .push(format!("{}: bad key in {:?}", layer.name(), item.get("on")));
                    continue;
                };
                let run_v = item.get("run").or_else(|| item.get("exec"));
                let run: Vec<Cmd> = match run_v {
                    Some(Value::String(s)) => Cmd::parse(s).into_iter().collect(),
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().and_then(Cmd::parse))
                        .collect(),
                    _ => Vec::new(),
                };
                let desc = item
                    .get("desc")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                list.push(Binding { on, run, desc });
            }
            km.layers.insert(layer, list);
        }
        km
    }

    pub fn bindings(&self, layer: Layer) -> &[Binding] {
        self.layers.get(&layer).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Resolve `keys` (the chord typed so far) in `layer`.
    pub fn lookup(&self, layer: Layer, keys: &[Key]) -> Lookup<'_> {
        let list = self.bindings(layer);
        let mut cands: Vec<&Binding> = Vec::new();
        for b in list {
            if b.on.len() >= keys.len() && b.on[..keys.len()] == *keys {
                if b.on.len() == keys.len() && cands.is_empty() {
                    return Lookup::Run(b);
                }
                if b.on.len() > keys.len() {
                    cands.push(b);
                }
            }
        }
        if cands.is_empty() {
            Lookup::None
        } else {
            Lookup::Prefix(cands)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_parse() {
        assert_eq!(Key::parse("a").unwrap().code, KeyCode::Char('a'));
        let k = Key::parse("<C-a>").unwrap();
        assert!(k.ctrl && k.code == KeyCode::Char('a'));
        let k = Key::parse("<S-Enter>").unwrap();
        assert!(k.shift && k.code == KeyCode::Enter);
        assert_eq!(Key::parse("<Space>").unwrap().code, KeyCode::Char(' '));
        assert_eq!(Key::parse("<F1>").unwrap().code, KeyCode::F(1));
        assert_eq!(Key::parse("<C-[>").unwrap().to_string(), "<C-[>");
        let k = Key::parse("<C-->").unwrap();
        assert!(k.ctrl && k.code == KeyCode::Char('-'));
        assert_eq!(Key::parse("<A-k>").unwrap().to_string(), "<A-k>");
    }

    #[test]
    fn cmd_parse() {
        let c = Cmd::parse("sort mtime --reverse=no --dir-first").unwrap();
        assert_eq!(c.name, "sort");
        assert_eq!(c.arg(0), Some("mtime"));
        assert_eq!(c.opt("reverse"), Some("no"));
        assert!(c.flag("dir-first"));
        let c = Cmd::parse("shell 'echo hi' --block").unwrap();
        assert_eq!(c.arg(0), Some("echo hi"));
        assert_eq!(c.layer(), (None, "shell"));
        assert_eq!(
            Cmd::parse("tasks:show").unwrap().layer(),
            (Some("tasks"), "show")
        );
    }

    #[test]
    fn preset_loads() {
        let t: Table = PRESET_KEYMAP.parse().unwrap();
        let km = Keymap::from_table(&t);
        assert!(km.warnings.is_empty(), "{:?}", km.warnings);
        let g = Key::parse("g").unwrap();
        match km.lookup(Layer::Mgr, &[g]) {
            Lookup::Prefix(c) => assert!(c.len() >= 5),
            _ => panic!("g is a chord prefix"),
        }
        let j = Key::parse("j").unwrap();
        assert!(matches!(km.lookup(Layer::Mgr, &[j]), Lookup::Run(_)));
    }
}

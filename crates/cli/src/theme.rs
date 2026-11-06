//! Theme, in yazi's `theme.toml` format (including flavors), plus the
//! filetype colouring rules and the nerd-font icon table.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::config::{config_dirs, find_user_file, merge, MimePattern, UrlPattern, PRESET_THEME};

/// Per-file flags used by `is = ...` filetype rules and icon conditions.
pub mod flags {
    pub const DIR: u16 = 1;
    pub const HIDDEN: u16 = 1 << 1;
    pub const LINK: u16 = 1 << 2;
    pub const ORPHAN: u16 = 1 << 3;
    pub const DUMMY: u16 = 1 << 4;
    pub const BLOCK: u16 = 1 << 5;
    pub const CHAR: u16 = 1 << 6;
    pub const FIFO: u16 = 1 << 7;
    pub const SOCK: u16 = 1 << 8;
    pub const EXEC: u16 = 1 << 9;
    pub const STICKY: u16 = 1 << 10;
    pub const HOVERED: u16 = 1 << 11;

    pub fn by_name(s: &str) -> Option<u16> {
        Some(match s {
            "dir" => DIR,
            "hidden" => HIDDEN,
            "link" => LINK,
            "orphan" => ORPHAN,
            "dummy" => DUMMY,
            "block" => BLOCK,
            "char" => CHAR,
            "fifo" => FIFO,
            "sock" => SOCK,
            "exec" => EXEC,
            "sticky" => STICKY,
            "hovered" => HOVERED,
            _ => return None,
        })
    }
}

/// A boolean expression over [`flags`]: `dir & hovered`, `!dir`, `(a | b)`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Cond {
    Flag(u16),
    Not(Box<Cond>),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
}

impl Cond {
    pub fn parse(s: &str) -> Option<Self> {
        let toks: Vec<String> = {
            let mut v = Vec::new();
            let mut cur = String::new();
            for c in s.chars() {
                if "!&|()".contains(c) || c.is_whitespace() {
                    if !cur.is_empty() {
                        v.push(std::mem::take(&mut cur));
                    }
                    if !c.is_whitespace() {
                        v.push(c.to_string());
                    }
                } else {
                    cur.push(c);
                }
            }
            if !cur.is_empty() {
                v.push(cur);
            }
            v
        };
        let mut pos = 0;
        let c = Self::or(&toks, &mut pos)?;
        (pos == toks.len()).then_some(c)
    }

    fn or(t: &[String], p: &mut usize) -> Option<Self> {
        let mut l = Self::and(t, p)?;
        while t.get(*p).map(String::as_str) == Some("|") {
            *p += 1;
            let r = Self::and(t, p)?;
            l = Cond::Or(Box::new(l), Box::new(r));
        }
        Some(l)
    }

    fn and(t: &[String], p: &mut usize) -> Option<Self> {
        let mut l = Self::unary(t, p)?;
        while t.get(*p).map(String::as_str) == Some("&") {
            *p += 1;
            let r = Self::unary(t, p)?;
            l = Cond::And(Box::new(l), Box::new(r));
        }
        Some(l)
    }

    fn unary(t: &[String], p: &mut usize) -> Option<Self> {
        let tok = t.get(*p)?;
        *p += 1;
        match tok.as_str() {
            "!" => Some(Cond::Not(Box::new(Self::unary(t, p)?))),
            "(" => {
                let c = Self::or(t, p)?;
                (t.get(*p).map(String::as_str) == Some(")")).then(|| {
                    *p += 1;
                    c
                })
            }
            name => flags::by_name(name).map(Cond::Flag),
        }
    }

    pub fn eval(&self, f: u16) -> bool {
        match self {
            Cond::Flag(x) => f & x != 0,
            Cond::Not(c) => !c.eval(f),
            Cond::And(a, b) => a.eval(f) && b.eval(f),
            Cond::Or(a, b) => a.eval(f) || b.eval(f),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Icon {
    pub text: String,
    #[serde(with = "style_serde")]
    pub style: Style,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Icons {
    globs: Vec<(UrlPattern, Icon)>,
    /// Sorted by name for binary search (cheaper to load than a hash map).
    dirs: Vec<(String, Icon)>,
    files: Vec<(String, Icon)>,
    exts: Vec<(String, Icon)>,
    conds: Vec<(Cond, Icon)>,
}

fn find<'a>(v: &'a [(String, Icon)], k: &str) -> Option<&'a Icon> {
    v.binary_search_by(|(n, _)| n.as_str().cmp(k))
        .ok()
        .map(|i| &v[i].1)
}

impl Icons {
    pub fn get(&self, path: &Path, name: &str, flags: u16) -> Option<&Icon> {
        let is_dir = flags & flags::DIR != 0;
        for (g, icon) in &self.globs {
            if g.matches(path, is_dir) {
                return Some(icon);
            }
        }
        let hit = if is_dir {
            find(&self.dirs, name)
        } else {
            find(&self.files, name)
                .or_else(|| find(&self.files, &name.to_lowercase()))
                .or_else(|| {
                    let ext = name.rsplit_once('.').map(|(_, e)| e)?;
                    find(&self.exts, ext).or_else(|| find(&self.exts, &ext.to_ascii_lowercase()))
                })
        };
        if hit.is_some() {
            return hit;
        }
        self.conds
            .iter()
            .find(|(c, _)| c.eval(flags))
            .map(|(_, i)| i)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiletypeRule {
    url: Option<UrlPattern>,
    mime: Option<MimePattern>,
    is: Option<Cond>,
    #[serde(with = "style_serde")]
    pub style: Style,
}

impl FiletypeRule {
    fn matches(&self, path: &Path, mime: &str, flags: u16) -> bool {
        let is_dir = flags & flags::DIR != 0;
        if let Some(c) = &self.is {
            if !c.eval(flags) {
                return false;
            }
        }
        if let Some(u) = &self.url {
            if !u.matches(path, is_dir) {
                return false;
            }
        }
        if let Some(m) = &self.mime {
            if mime.is_empty() || !m.matches(mime) {
                return false;
            }
        }
        self.url.is_some() || self.mime.is_some()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Theme {
    #[serde(with = "style_serde::map")]
    styles: HashMap<String, Style>,
    strings: HashMap<String, String>,
    pub filetypes: Vec<FiletypeRule>,
    pub icons: Icons,
    /// A `.tmTheme` for syntax highlighting, if the theme names one.
    pub syntect_theme: Option<PathBuf>,
    pub warnings: Vec<String>,
    /// Extra files this theme was built from (flavors), for cache validation.
    pub deps: Vec<PathBuf>,
}

/// Compact (de)serialization of ratatui styles for the config cache; ratatui's
/// own serde impl needs a self-describing format.
mod style_serde {
    use ratatui::style::{Color, Modifier, Style};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    fn enc(c: Option<Color>) -> u32 {
        const NAMED: [Color; 16] = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        match c {
            None => 0,
            Some(Color::Reset) => 1,
            Some(Color::Indexed(i)) => 0x100 | i as u32,
            Some(Color::Rgb(r, g, b)) => {
                0x0100_0000 | (r as u32) << 16 | (g as u32) << 8 | b as u32
            }
            Some(other) => 2 + NAMED.iter().position(|n| *n == other).unwrap_or(0) as u32,
        }
    }

    fn dec(v: u32) -> Option<Color> {
        const NAMED: [Color; 16] = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        match v {
            0 => None,
            1 => Some(Color::Reset),
            v if v & 0x0100_0000 != 0 => Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)),
            v if v & 0x100 != 0 => Some(Color::Indexed(v as u8)),
            v => NAMED.get(v as usize - 2).copied(),
        }
    }

    type Raw = (u32, u32, u16, u16);

    fn to_raw(s: &Style) -> Raw {
        (
            enc(s.fg),
            enc(s.bg),
            s.add_modifier.bits(),
            s.sub_modifier.bits(),
        )
    }

    fn from_raw(r: Raw) -> Style {
        Style {
            fg: dec(r.0),
            bg: dec(r.1),
            add_modifier: Modifier::from_bits_truncate(r.2),
            sub_modifier: Modifier::from_bits_truncate(r.3),
            ..Style::default()
        }
    }

    pub fn serialize<S: Serializer>(s: &Style, ser: S) -> Result<S::Ok, S::Error> {
        to_raw(s).serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Style, D::Error> {
        Raw::deserialize(d).map(from_raw)
    }

    pub mod map {
        use super::*;
        use std::collections::HashMap;

        pub fn serialize<S: Serializer>(
            m: &HashMap<String, Style>,
            ser: S,
        ) -> Result<S::Ok, S::Error> {
            let v: Vec<(&String, Raw)> = m.iter().map(|(k, s)| (k, to_raw(s))).collect();
            v.serialize(ser)
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<HashMap<String, Style>, D::Error> {
            let v: Vec<(String, Raw)> = Vec::deserialize(d)?;
            Ok(v.into_iter().map(|(k, r)| (k, from_raw(r))).collect())
        }
    }
}

pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let v = match hex.len() {
            6 => u32::from_str_radix(hex, 16).ok()?,
            3 => {
                let v = u32::from_str_radix(hex, 16).ok()?;
                let (r, g, b) = ((v >> 8) & 0xf, (v >> 4) & 0xf, v & 0xf);
                (r * 17) << 16 | (g * 17) << 8 | (b * 17)
            }
            _ => return None,
        };
        return Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8));
    }
    Some(match s.to_ascii_lowercase().as_str() {
        "reset" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        n => Color::Indexed(n.parse().ok()?),
    })
}

const STYLE_KEYS: &[&str] = &[
    "fg",
    "bg",
    "bold",
    "dim",
    "italic",
    "underline",
    "blink",
    "blink_rapid",
    "reversed",
    "hidden",
    "crossed",
];

fn looks_like_style(t: &Table) -> bool {
    t.keys().all(|k| STYLE_KEYS.contains(&k.as_str()))
}

pub fn parse_style(t: &Table) -> Style {
    let mut s = Style::default();
    if let Some(c) = t.get("fg").and_then(Value::as_str).and_then(parse_color) {
        s = s.fg(c);
    }
    if let Some(c) = t.get("bg").and_then(Value::as_str).and_then(parse_color) {
        s = s.bg(c);
    }
    for (k, m) in [
        ("bold", Modifier::BOLD),
        ("dim", Modifier::DIM),
        ("italic", Modifier::ITALIC),
        ("underline", Modifier::UNDERLINED),
        ("blink", Modifier::SLOW_BLINK),
        ("blink_rapid", Modifier::RAPID_BLINK),
        ("reversed", Modifier::REVERSED),
        ("hidden", Modifier::HIDDEN),
        ("crossed", Modifier::CROSSED_OUT),
    ] {
        match t.get(k).and_then(Value::as_bool) {
            Some(true) => s = s.add_modifier(m),
            Some(false) => s = s.remove_modifier(m),
            None => {}
        }
    }
    s
}

fn icon_of(t: &Table) -> Option<Icon> {
    let text = t.get("text").and_then(Value::as_str)?.to_string();
    Some(Icon {
        text,
        style: parse_style(t),
    })
}

impl Theme {
    pub fn load() -> Self {
        let mut base: Table = PRESET_THEME.parse().expect("preset theme is valid");
        let mut warnings = Vec::new();
        let user: Option<Table> = find_user_file("theme.toml").and_then(|p| {
            match std::fs::read_to_string(&p)
                .map_err(anyhow::Error::from)
                .and_then(|s| s.parse::<Table>().map_err(anyhow::Error::from))
            {
                Ok(t) => Some(t),
                Err(e) => {
                    warnings.push(format!("{}: {e}", p.display()));
                    None
                }
            }
        });
        let mut syntect_theme = None;
        let mut deps = Vec::new();
        // Flavor: `[flavor] dark = "name"` → `flavors/name.yazi/flavor.toml`.
        let flavor = user
            .as_ref()
            .and_then(|u| u.get("flavor"))
            .and_then(|f| f.get("dark").or_else(|| f.get("use")))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from);
        if let Some(name) = flavor {
            let dir = config_dirs()
                .into_iter()
                .map(|d| d.join("flavors").join(format!("{name}.yazi")))
                .find(|d| d.is_dir());
            match dir {
                Some(dir) => {
                    deps.push(dir.join("flavor.toml"));
                    deps.push(dir.join("tmtheme.xml"));
                    if let Ok(t) = std::fs::read_to_string(dir.join("flavor.toml"))
                        .map_err(anyhow::Error::from)
                        .and_then(|s| s.parse::<Table>().map_err(anyhow::Error::from))
                    {
                        merge(&mut base, t);
                    }
                    let tm = dir.join("tmtheme.xml");
                    if tm.is_file() {
                        syntect_theme = Some(tm);
                    }
                }
                None => warnings.push(format!("flavor `{name}` not found")),
            }
        }
        if let Some(u) = user {
            merge(&mut base, u);
        }
        let mut th = Self::from_table(&base);
        if let Some(s) = th
            .strings
            .get("mgr.syntect_theme")
            .filter(|s| !s.is_empty())
        {
            let p = crate::util::expand_path(s);
            if p.is_file() {
                syntect_theme = Some(p);
            }
        }
        th.syntect_theme = syntect_theme;
        th.deps = deps;
        th.warnings.extend(warnings);
        th
    }

    pub fn from_table(t: &Table) -> Self {
        let mut th = Theme::default();
        for (section, v) in t {
            let Value::Table(sec) = v else { continue };
            if section == "icon" || section == "filetype" || section == "flavor" {
                continue;
            }
            for (k, v) in sec {
                let key = format!("{section}.{k}");
                match v {
                    Value::Table(st) if looks_like_style(st) => {
                        th.styles.insert(key, parse_style(st));
                    }
                    Value::Table(st) => {
                        for (k2, v2) in st {
                            if let Some(s) = v2.as_str() {
                                th.strings.insert(format!("{key}.{k2}"), s.to_string());
                            }
                        }
                    }
                    Value::String(s) => {
                        th.strings.insert(key, s.clone());
                    }
                    Value::Integer(i) => {
                        th.strings.insert(key, i.to_string());
                    }
                    Value::Array(a) => {
                        for (i, x) in a.iter().enumerate() {
                            if let Some(s) = x.as_str() {
                                th.strings.insert(format!("{key}.{i}"), s.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let rules = t
            .get("filetype")
            .and_then(|f| f.get("rules"))
            .and_then(Value::as_array);
        for r in rules.into_iter().flatten() {
            let Some(r) = r.as_table() else { continue };
            let url = r
                .get("url")
                .or_else(|| r.get("name"))
                .and_then(Value::as_str)
                .and_then(UrlPattern::new);
            let mime = r
                .get("mime")
                .and_then(Value::as_str)
                .and_then(MimePattern::new);
            let is = r.get("is").and_then(Value::as_str).and_then(Cond::parse);
            th.filetypes.push(FiletypeRule {
                url,
                mime,
                is,
                style: parse_style(r),
            });
        }

        if let Some(Value::Table(ic)) = t.get("icon") {
            let list = |k: &str| {
                ic.get(k)
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default()
            };
            for g in list("globs") {
                let Some(g) = g.as_table() else { continue };
                let pat = g
                    .get("url")
                    .or_else(|| g.get("name"))
                    .and_then(Value::as_str)
                    .and_then(UrlPattern::new);
                if let (Some(p), Some(i)) = (pat, icon_of(g)) {
                    th.icons.globs.push((p, i));
                }
            }
            for (k, map) in [
                ("dirs", &mut th.icons.dirs),
                ("files", &mut th.icons.files),
                ("exts", &mut th.icons.exts),
            ] {
                for e in list(k) {
                    let Some(e) = e.as_table() else { continue };
                    if let (Some(n), Some(i)) = (e.get("name").and_then(Value::as_str), icon_of(e))
                    {
                        map.push((n.to_string(), i));
                    }
                }
                // Sort, keeping the first definition of duplicate names.
                map.sort_by(|a, b| a.0.cmp(&b.0));
                map.dedup_by(|a, b| a.0 == b.0);
            }
            for c in list("conds") {
                let Some(c) = c.as_table() else { continue };
                let cond = c.get("if").and_then(Value::as_str).and_then(Cond::parse);
                if let (Some(cd), Some(i)) = (cond, icon_of(c)) {
                    th.icons.conds.push((cd, i));
                }
            }
        }
        th
    }

    /// Every named style and string, for exposing the theme to plugins.
    pub fn entries(&self) -> (&HashMap<String, Style>, &HashMap<String, String>) {
        (&self.styles, &self.strings)
    }

    /// A named style, e.g. `th.s("mgr.cwd")`. Missing → default.
    pub fn s(&self, key: &str) -> Style {
        self.styles.get(key).copied().unwrap_or_default()
    }

    /// A named string, e.g. `th.str("mgr.border_symbol")`.
    pub fn str<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.strings.get(key).map(String::as_str).unwrap_or(default)
    }

    /// The style for a file from the `[filetype]` rules.
    pub fn file_style(&self, path: &Path, mime: &str, flags: u16) -> Style {
        self.filetypes
            .iter()
            .find(|r| r.matches(path, mime, flags))
            .map(|r| r.style)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_theme() {
        let t: Table = PRESET_THEME.parse().unwrap();
        let th = Theme::from_table(&t);
        assert_eq!(th.s("mgr.cwd").fg, Some(Color::Cyan));
        assert_eq!(th.str("mgr.border_symbol", "?"), "│");
        assert_eq!(th.str("tabs.sep_inner.open", "?"), "\u{e0b6}");
        let dir = th.file_style(Path::new("/tmp/x"), "inode/directory", flags::DIR);
        assert_eq!(dir.fg, Some(Color::Blue));
        let img = th.file_style(Path::new("/tmp/x.png"), "image/png", 0);
        assert_eq!(img.fg, Some(Color::Yellow));
        let i = th.icons.get(Path::new("/a/main.rs"), "main.rs", 0).unwrap();
        assert!(!i.text.is_empty());
        let d = th
            .icons
            .get(Path::new("/a/zzz"), "zzz", flags::DIR)
            .unwrap();
        assert!(!d.text.is_empty());
    }

    #[test]
    fn conds() {
        let c = Cond::parse("dir & !hovered").unwrap();
        assert!(c.eval(flags::DIR));
        assert!(!c.eval(flags::DIR | flags::HOVERED));
        let c = Cond::parse("(link | orphan) & !dir").unwrap();
        assert!(c.eval(flags::LINK));
    }
}

//! yazi-compatible Lua plugins.
//!
//! Plugins live in `plugins/<name>.yazi/main.lua` under the config dirs (see
//! [`crate::config::config_dirs`]) and run through yazi's plugin API: `ya`,
//! `Command`, `Url`, `fs`, `ps`, `cx`, plus no-op `ui`/`th`/`Status`/`Header`
//! stand-ins so `init.lua` UI tweaks load without errors.
//!
//! The Lua state is created only when a plugin (or `init.lua`) is first used,
//! so start-up pays nothing for it. A plugin runs as a coroutine on the UI
//! thread; anything that needs the user — `ya.input`, `ya.confirm`, `ya.which`,
//! `ya.hide` — yields to the app, which shows its own popup (or hands the
//! terminal over) and resumes the coroutine with the answer. Commands sent
//! with `ya.emit` are queued and run by the app between steps.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};

use mlua::thread::ThreadStatus;
use mlua::{Function, Lua, LuaSerdeExt, LuaString, MultiValue, Table, Thread, Value};

use crate::app::Level;
use crate::config::{config_dirs, Config};
use crate::fs::File;
use crate::keymap::{Cmd, Key};

const PRELUDE: &str = include_str!("plugin_prelude.lua");
const UI: &str = include_str!("plugin_ui.lua");

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Spot rows from a Lua spotter (`None`: use the built-in table).
pub type SpotResult = Result<Option<Vec<(String, String)>>, String>;

/// A styled run of text from a Lua widget.
pub type Spans = Vec<(String, Style)>;

/// Where a plugin's entry file is, if installed.
pub fn find_plugin(name: &str) -> Option<PathBuf> {
    for d in config_dirs() {
        let dir = d.join("plugins").join(format!("{name}.yazi"));
        for f in ["main.lua", "init.lua"] {
            let p = dir.join(f);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// The user's `init.lua`, if any.
pub fn find_init() -> Option<PathBuf> {
    config_dirs()
        .into_iter()
        .map(|d| d.join("init.lua"))
        .find(|p| p.is_file())
}

/// A snapshot of app state for `cx`.
pub struct Snapshot {
    /// Screen areas by name (`preview`, `current`, `parent`) for `ui.area`.
    pub areas: Vec<(&'static str, ratatui::layout::Rect)>,
    pub tabs: Vec<TabSnap>,
    pub cur: usize,
    pub yanked: Vec<PathBuf>,
    pub cut: bool,
}

pub struct TabSnap {
    pub name: String,
    pub cwd: PathBuf,
    pub current: FolderSnap,
    pub parent: Option<FolderSnap>,
    pub preview: Option<FolderSnap>,
    pub preview_skip: usize,
    pub selected: Vec<PathBuf>,
    pub mode: &'static str,
    pub visual_start: usize,
    pub sort_by: String,
    pub sort_reverse: bool,
    pub sort_dir_first: bool,
    pub sort_sensitive: bool,
    pub linemode: String,
    pub show_hidden: bool,
}

pub struct FolderSnap {
    pub cwd: PathBuf,
    pub files: Vec<File>,
    pub cursor: usize,
    pub offset: usize,
    pub window: std::ops::Range<usize>,
    pub hovered: Option<usize>,
}

/// What a plugin step ended with.
pub enum Step {
    Done,
    Input {
        title: String,
        value: String,
        obscure: bool,
    },
    /// `recv()` on a realtime `ya.input`; `open` on the first call.
    InputRecv {
        title: String,
        value: String,
        obscure: bool,
        open: bool,
    },
    Confirm {
        title: String,
        body: String,
    },
    /// `ya.which`: candidates, and whether to show the popup.
    Which(Vec<(Vec<Key>, String)>, bool),
    Hide,
    Show,
    Error(String),
}

/// How to resume a suspended plugin.
pub enum Resume {
    Nothing,
    Input(Option<String>),
    /// A realtime input changed (event 3).
    Changed(String),
    Bool(bool),
    Index(Option<usize>),
}

struct Proc {
    child: Child,
    out: Option<BufReader<ChildStdout>>,
    err: Option<BufReader<ChildStderr>>,
}

#[derive(Default)]
struct Shared {
    emits: Vec<Cmd>,
    notes: Vec<(Level, String, String)>,
    procs: HashMap<i64, Proc>,
    next_proc: i64,
    hidden: bool,
    /// `ui.render()` was called.
    render: bool,
    /// Components were (re)registered: cached flags are stale.
    components: u64,
}

pub struct Runtime {
    lua: Lua,
    running: Option<(String, Thread)>,
    pub hidden: bool,
    active: std::cell::RefCell<(u64, HashMap<String, bool>)>,
}

/// What a file-bound component call is about.
pub struct FileCtx<'a> {
    pub file: &'a File,
    pub hovered: bool,
    pub in_current: bool,
    pub in_preview: bool,
    pub in_parent: bool,
}

/// A Tab layout computed by Lua: pane chunks plus border widgets
/// `(area, type, style, merge)` (type: 0 plain, 1 rounded, 2 double, 3 thick).
#[derive(Clone, Debug, Default)]
pub struct TabLayout {
    pub chunks: [ratatui::layout::Rect; 3],
    pub borders: Vec<(ratatui::layout::Rect, i64, Style, bool)>,
}

/// What a Lua previewer produced.
#[derive(Default)]
pub struct PeekOut {
    pub lines: Option<Vec<Line<'static>>>,
    pub image: Option<PathBuf>,
    pub native: Option<String>,
    pub peek: Option<usize>,
    pub error: Option<String>,
}

fn color_name(c: Color) -> String {
    match c {
        Color::Reset => "reset".into(),
        Color::Black => "black".into(),
        Color::Red => "red".into(),
        Color::Green => "green".into(),
        Color::Yellow => "yellow".into(),
        Color::Blue => "blue".into(),
        Color::Magenta => "magenta".into(),
        Color::Cyan => "cyan".into(),
        Color::Gray => "gray".into(),
        Color::DarkGray => "darkgray".into(),
        Color::LightRed => "lightred".into(),
        Color::LightGreen => "lightgreen".into(),
        Color::LightYellow => "lightyellow".into(),
        Color::LightBlue => "lightblue".into(),
        Color::LightMagenta => "lightmagenta".into(),
        Color::LightCyan => "lightcyan".into(),
        Color::White => "white".into(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(i) => i.to_string(),
    }
}

const MODS: [(&str, Modifier); 9] = [
    ("bold", Modifier::BOLD),
    ("dim", Modifier::DIM),
    ("italic", Modifier::ITALIC),
    ("underline", Modifier::UNDERLINED),
    ("blink", Modifier::SLOW_BLINK),
    ("blink_rapid", Modifier::RAPID_BLINK),
    ("reverse", Modifier::REVERSED),
    ("hidden", Modifier::HIDDEN),
    ("crossed", Modifier::CROSSED_OUT),
];

fn style_to_lua(lua: &Lua, s: &Style) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    if let Some(c) = s.fg {
        t.set("fg", color_name(c))?;
    }
    if let Some(c) = s.bg {
        t.set("bg", color_name(c))?;
    }
    let mods = lua.create_table()?;
    for (n, m) in MODS {
        if s.add_modifier.contains(m) {
            mods.set(n, true)?;
        }
    }
    t.set("mods", mods)?;
    Ok(t)
}

/// A `ui.Style` table → ratatui style.
fn lua_style(v: &Value) -> Style {
    let Value::Table(t) = v else {
        return Style::default();
    };
    let mut s = Style::default();
    let col = |k: &str| {
        t.get::<Option<String>>(k)
            .ok()
            .flatten()
            .and_then(|c| crate::theme::parse_color(&c))
    };
    if let Some(c) = col("_fg") {
        s = s.fg(c);
    }
    if let Some(c) = col("_bg") {
        s = s.bg(c);
    }
    if let Ok(mods) = t.get::<Table>("mods") {
        for (n, m) in MODS {
            match mods.get::<Option<bool>>(n).ok().flatten() {
                Some(true) => s = s.add_modifier(m),
                Some(false) => s = s.remove_modifier(m),
                None => {}
            }
        }
    }
    s
}

fn tag(t: &Table) -> String {
    t.get::<Option<String>>("_t")
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Flatten a span/line/string (or an array of them) into styled runs.
pub fn widget_spans(v: &Value, base: Style, out: &mut Spans) {
    match v {
        Value::String(s) => out.push((s.to_string_lossy(), base)),
        Value::Integer(i) => out.push((i.to_string(), base)),
        Value::Number(n) => out.push((n.to_string(), base)),
        Value::Table(t) => {
            let st = base.patch(lua_style(&t.get::<Value>("_style").unwrap_or(Value::Nil)));
            match tag(t).as_str() {
                "span" => {
                    let text: String = t
                        .get::<Option<String>>("text")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    out.push((text, st));
                }
                "line" => {
                    if let Ok(spans) = t.get::<Table>("spans") {
                        for s in spans.sequence_values::<Value>().flatten() {
                            widget_spans(&s, st, out);
                        }
                    }
                }
                "text" => {
                    if let Ok(lines) = t.get::<Table>("lines") {
                        if let Some(Ok(l)) = lines.sequence_values::<Value>().next() {
                            widget_spans(&l, st, out);
                        }
                    }
                }
                "" => {
                    // An object with `__tostring` (a Url, …) is text.
                    let ts = t
                        .metatable()
                        .and_then(|m| m.get::<Option<Function>>("__tostring").ok().flatten());
                    if let Some(f) = ts {
                        if let Ok(s) = f.call::<String>(t.clone()) {
                            out.push((s, base));
                        }
                        return;
                    }
                    for s in t.sequence_values::<Value>().flatten() {
                        widget_spans(&s, base, out);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn spans_to_line(s: Spans) -> Line<'static> {
    Line::from(
        s.into_iter()
            .map(|(t, st)| Span::styled(t, st))
            .collect::<Vec<_>>(),
    )
}

fn spans_width(s: &[(String, Style)]) -> usize {
    s.iter().map(|(t, _)| crate::util::width(t)).sum()
}

/// One widget → lines of `width` cells (alignment: 0 left, 1 center, 2 right).
fn one_widget(t: &Table, width: usize) -> (Vec<Spans>, i64) {
    let base = lua_style(&t.get::<Value>("_style").unwrap_or(Value::Nil));
    let align = t.get::<Option<i64>>("_align").ok().flatten().unwrap_or(0);
    let wrap = t.get::<Option<i64>>("_wrap").ok().flatten().unwrap_or(0) > 0;
    let mut rows: Vec<Spans> = Vec::new();
    match tag(t).as_str() {
        "text" | "list" => {
            let key = if tag(t) == "text" { "lines" } else { "items" };
            if let Ok(lines) = t.get::<Table>(key) {
                for l in lines.sequence_values::<Value>().flatten() {
                    let mut s = Vec::new();
                    widget_spans(&l, base, &mut s);
                    if wrap && spans_width(&s) > width && width > 0 {
                        let plain: String = s.iter().map(|x| x.0.as_str()).collect();
                        let st = s.first().map(|x| x.1).unwrap_or(base);
                        for w in crate::preview::word_wrap(&plain, width) {
                            rows.push(vec![(w, st)]);
                        }
                    } else {
                        rows.push(s);
                    }
                }
            }
        }
        "table" => {
            let widths: Vec<usize> = t
                .get::<Table>("_widths")
                .map(|w| {
                    w.sequence_values::<Table>()
                        .flatten()
                        .map(|c| c.get::<Option<usize>>("a").ok().flatten().unwrap_or(0))
                        .collect()
                })
                .unwrap_or_default();
            if let Ok(trows) = t.get::<Table>("rows") {
                let mut cells_all: Vec<(Vec<Spans>, Style)> = Vec::new();
                for r in trows.sequence_values::<Table>().flatten() {
                    let rs = base.patch(lua_style(&r.get::<Value>("_style").unwrap_or(Value::Nil)));
                    let mut cells = Vec::new();
                    if let Ok(cs) = r.get::<Table>("cells") {
                        for c in cs.sequence_values::<Value>().flatten() {
                            let mut s = Vec::new();
                            match &c {
                                Value::Table(ct) if tag(ct).is_empty() && ct.raw_len() > 0 => {
                                    // A cell given as a list of lines: first line.
                                    if let Some(Ok(first)) = ct.sequence_values::<Value>().next() {
                                        widget_spans(&first, rs, &mut s);
                                    }
                                }
                                other => widget_spans(other, rs, &mut s),
                            }
                            cells.push(s);
                        }
                    }
                    cells_all.push((cells, rs));
                }
                let ncols = cells_all.iter().map(|c| c.0.len()).max().unwrap_or(0);
                let mut colw: Vec<usize> = (0..ncols)
                    .map(|i| {
                        widths
                            .get(i)
                            .copied()
                            .filter(|w| *w > 0)
                            .unwrap_or_else(|| {
                                cells_all
                                    .iter()
                                    .filter_map(|c| c.0.get(i))
                                    .map(|s| spans_width(s) + 1)
                                    .max()
                                    .unwrap_or(0)
                            })
                    })
                    .collect();
                if let Some(last) = colw.last_mut() {
                    *last = 0;
                }
                for (cells, _) in cells_all {
                    let mut row: Spans = Vec::new();
                    for (i, c) in cells.into_iter().enumerate() {
                        let w = spans_width(&c);
                        row.extend(c);
                        let target = colw.get(i).copied().unwrap_or(0);
                        if target > w {
                            row.push((" ".repeat(target - w), Style::default()));
                        }
                    }
                    rows.push(row);
                }
            }
        }
        "gauge" => {
            let mut s = Vec::new();
            if let Ok(l) = t.get::<Value>("_label") {
                widget_spans(&l, base, &mut s);
            }
            rows.push(s);
        }
        "clear" | "bar" | "border" => {}
        _ => {
            let mut s = Vec::new();
            widget_spans(&Value::Table(t.clone()), Style::default(), &mut s);
            rows.push(s);
        }
    }
    (rows, align)
}

/// A positioned drawing operation from Lua widgets (plugin UIs, list
/// redraw overrides).
#[derive(Clone, Debug)]
pub enum Draw {
    Clear(ratatui::layout::Rect),
    Border {
        area: ratatui::layout::Rect,
        kind: i64,
        style: Style,
        title: Option<(Spans, i64)>,
    },
    /// Rows of text: `(spans, row fill style)`, with an alignment.
    Lines {
        area: ratatui::layout::Rect,
        rows: Vec<(Spans, Style)>,
        align: i64,
    },
    Bar {
        area: ratatui::layout::Rect,
        symbol: String,
        style: Style,
    },
}

fn lua_rect(v: &Value) -> Option<ratatui::layout::Rect> {
    let Value::Table(t) = v else { return None };
    let n = |k: &str| {
        t.get::<Option<f64>>(k)
            .ok()
            .flatten()
            .map(|v| v.max(0.0) as u16)
    };
    Some(ratatui::layout::Rect::new(
        n("x")?,
        n("y")?,
        n("w")?,
        n("h")?,
    ))
}

/// A table widget's rows, honouring header, widths (Length/Percentage),
/// the selected row (`:row(i)`, 0-based) and `row_style`.
fn table_rows(t: &Table, width: usize) -> Vec<(Spans, Style)> {
    let base = lua_style(&t.get::<Value>("_style").unwrap_or(Value::Nil));
    let widths: Vec<usize> = t
        .get::<Table>("_widths")
        .map(|w| {
            w.sequence_values::<Table>()
                .flatten()
                .map(|c| {
                    let kind: String = c
                        .get::<Option<String>>("kind")
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let a = c.get::<Option<f64>>("a").ok().flatten().unwrap_or(0.0) as usize;
                    match kind.as_str() {
                        "Percentage" => width * a / 100,
                        _ => a,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let cells_of = |r: &Table, st: Style| -> Vec<Spans> {
        let mut cells = Vec::new();
        if let Ok(cs) = r.get::<Table>("cells") {
            for c in cs.sequence_values::<Value>().flatten() {
                let mut s = Vec::new();
                match &c {
                    Value::Table(ct) if tag(ct).is_empty() && ct.raw_len() > 0 => {
                        if let Some(Ok(first)) = ct.sequence_values::<Value>().next() {
                            widget_spans(&first, st, &mut s);
                        }
                    }
                    other => widget_spans(other, st, &mut s),
                }
                cells.push(s);
            }
        }
        cells
    };
    let mut all: Vec<(Vec<Spans>, Style)> = Vec::new();
    if let Ok(h) = t.get::<Table>("_header") {
        let st = base.patch(lua_style(&h.get::<Value>("_style").unwrap_or(Value::Nil)));
        all.push((cells_of(&h, st), Style::default()));
    }
    let header_rows = all.len();
    let selected = t.get::<Option<i64>>("_row").ok().flatten();
    let sel_style = lua_style(&t.get::<Value>("_row_style").unwrap_or(Value::Nil));
    if let Ok(rows) = t.get::<Table>("rows") {
        for (i, r) in rows.sequence_values::<Table>().flatten().enumerate() {
            let mut st = base.patch(lua_style(&r.get::<Value>("_style").unwrap_or(Value::Nil)));
            let mut fill = Style::default();
            if selected == Some(i as i64) {
                st = st.patch(sel_style);
                fill = sel_style;
            }
            all.push((cells_of(&r, st), fill));
        }
    }
    let ncols = all.iter().map(|c| c.0.len()).max().unwrap_or(0);
    let colw: Vec<usize> = (0..ncols)
        .map(|i| {
            widths
                .get(i)
                .copied()
                .filter(|w| *w > 0)
                .unwrap_or_else(|| {
                    all.iter()
                        .filter_map(|c| c.0.get(i))
                        .map(|s| spans_width(s) + 1)
                        .max()
                        .unwrap_or(0)
                })
        })
        .collect();
    let _ = header_rows;
    all.into_iter()
        .map(|(cells, fill)| {
            let mut row: Spans = Vec::new();
            let last = cells.len().saturating_sub(1);
            for (i, c) in cells.into_iter().enumerate() {
                let w = spans_width(&c);
                let target = colw.get(i).copied().unwrap_or(0);
                // Clip a cell to its column, then pad (spacing 1 like ratatui).
                let mut used = 0;
                for (txt, st) in c {
                    if target > 0 && used >= target.saturating_sub(1) && i != last {
                        break;
                    }
                    let room = if target > 0 && i != last {
                        target.saturating_sub(1).saturating_sub(used)
                    } else {
                        usize::MAX
                    };
                    let piece = crate::util::truncate(&txt, room).to_string();
                    used += crate::util::width(&piece);
                    row.push((piece, st));
                }
                let _ = w;
                if i != last && target > used {
                    row.push((" ".repeat(target - used), Style::default()));
                }
            }
            (row, fill)
        })
        .collect()
}

/// Turn Lua widgets (one, or an array) into positioned drawing operations.
pub fn widget_draws(v: &Value) -> Vec<Draw> {
    let widgets: Vec<Table> = match v {
        Value::Table(t) if tag(t).is_empty() => t.sequence_values::<Table>().flatten().collect(),
        Value::Table(t) => vec![t.clone()],
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for w in widgets {
        let Some(area) = lua_rect(&w.get::<Value>("_area").unwrap_or(Value::Nil)) else {
            continue;
        };
        let style = lua_style(&w.get::<Value>("_style").unwrap_or(Value::Nil));
        match tag(&w).as_str() {
            "clear" => out.push(Draw::Clear(area)),
            "border" => {
                let title = match w.get::<Value>("_title").unwrap_or(Value::Nil) {
                    Value::Nil => None,
                    tv => {
                        let mut s = Vec::new();
                        widget_spans(&tv, Style::default(), &mut s);
                        let align = match &tv {
                            Value::Table(t) => {
                                t.get::<Option<i64>>("_align").ok().flatten().unwrap_or(0)
                            }
                            _ => 0,
                        };
                        Some((s, align))
                    }
                };
                out.push(Draw::Border {
                    area,
                    kind: w.get::<Option<i64>>("_type").ok().flatten().unwrap_or(0),
                    style,
                    title,
                });
            }
            "bar" => out.push(Draw::Bar {
                area,
                symbol: w
                    .get::<Option<String>>("_symbol")
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "│".into()),
                style,
            }),
            "table" => out.push(Draw::Lines {
                area,
                rows: table_rows(&w, area.width as usize),
                align: 0,
            }),
            "list" => {
                let mut rows = Vec::new();
                if let Ok(items) = w.get::<Table>("items") {
                    for it in items.sequence_values::<Value>().flatten() {
                        // A line's own style fills its whole row (ratatui List).
                        let fill = match &it {
                            Value::Table(t) => {
                                lua_style(&t.get::<Value>("_style").unwrap_or(Value::Nil))
                            }
                            _ => Style::default(),
                        };
                        let mut s = Vec::new();
                        widget_spans(&it, style, &mut s);
                        rows.push((s, fill));
                    }
                }
                out.push(Draw::Lines {
                    area,
                    rows,
                    align: 0,
                });
            }
            _ => {
                let (rows, align) = one_widget(&w, area.width as usize);
                out.push(Draw::Lines {
                    area,
                    rows: rows.into_iter().map(|r| (r, Style::default())).collect(),
                    align,
                });
            }
        }
    }
    out
}

/// A widget, or an array of widgets drawn over each other (e.g. a list with a
/// right-aligned column), → lines for a `width`-cell pane.
pub fn widget_lines(v: &Value, width: usize) -> Vec<Line<'static>> {
    let widgets: Vec<Table> = match v {
        Value::Table(t) if tag(t).is_empty() => t.sequence_values::<Table>().flatten().collect(),
        Value::Table(t) => vec![t.clone()],
        Value::String(s) => {
            return s
                .to_string_lossy()
                .lines()
                .map(|l| Line::raw(l.to_string()))
                .collect();
        }
        _ => return Vec::new(),
    };
    let mut grid: Vec<(Spans, Spans)> = Vec::new();
    for w in widgets {
        let (rows, align) = one_widget(&w, width);
        for (i, r) in rows.into_iter().enumerate() {
            if grid.len() <= i {
                grid.push((Vec::new(), Vec::new()));
            }
            if align == 2 {
                grid[i].1.extend(r);
            } else if grid[i].0.is_empty() {
                grid[i].0 = r;
            }
        }
    }
    grid.into_iter()
        .map(|(mut l, r)| {
            if !r.is_empty() {
                let lw = spans_width(&l);
                let rw = spans_width(&r);
                if width > lw + rw {
                    l.push((" ".repeat(width - lw - rw), Style::default()));
                }
                l.extend(r);
            }
            spans_to_line(l)
        })
        .collect()
}

fn s<T: ToString>(v: T) -> String {
    v.to_string()
}

fn val_to_string(lua: &Lua, v: &Value) -> String {
    match v {
        Value::String(x) => x.to_string_lossy(),
        Value::Boolean(b) => {
            if *b {
                "yes".into()
            } else {
                "no".into()
            }
        }
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Nil => String::new(),
        other => {
            let ts: Option<Function> = lua.globals().get("tostring").ok();
            ts.and_then(|f| f.call::<String>(other.clone()).ok())
                .unwrap_or_default()
        }
    }
}

fn status_table(lua: &Lua, st: std::process::ExitStatus) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("success", st.success())?;
    t.set("code", st.code().map(i64::from))?;
    Ok(t)
}

fn stdio(mode: i64, hidden: bool) -> Stdio {
    match mode {
        0 => Stdio::null(),
        1 => Stdio::piped(),
        2 => Stdio::inherit(),
        _ if hidden => Stdio::inherit(),
        _ => Stdio::null(),
    }
}

fn build_command(spec: &Table, hidden: bool, piped_out: bool) -> mlua::Result<Command> {
    let program: String = spec.get("program")?;
    let mut c = Command::new(program);
    if let Ok(args) = spec.get::<Table>("_args") {
        for a in args.sequence_values::<String>() {
            c.arg(a?);
        }
    }
    if let Ok(Some(cwd)) = spec.get::<Option<String>>("_cwd") {
        c.current_dir(cwd);
    }
    if let Ok(env) = spec.get::<Table>("_env") {
        for pair in env.pairs::<String, String>() {
            let (k, v) = pair?;
            c.env(k, v);
        }
    }
    let m = |k: &str| spec.get::<Option<i64>>(k).ok().flatten().unwrap_or(-1);
    c.stdin(stdio(m("_stdin"), hidden));
    if piped_out {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        c.stdout(stdio(m("_stdout"), hidden));
        c.stderr(stdio(m("_stderr"), hidden));
    }
    Ok(c)
}

fn raw_file(lua: &Lua, f: &File) -> mlua::Result<Table> {
    use crate::theme::flags as fl;
    let t = lua.create_table()?;
    let c = &f.cha;
    t.set("name", f.name.to_string())?;
    t.set("flags", c.flags)?;
    t.set("mime", f.quick_mime().unwrap_or(""))?;
    t.set("path", f.url.to_string_lossy().into_owned())?;
    t.set("is_dir", c.is_dir())?;
    t.set("is_hidden", f.is_hidden())?;
    t.set("is_link", c.flags & fl::LINK != 0)?;
    t.set("is_orphan", c.flags & fl::ORPHAN != 0)?;
    t.set("is_exec", c.flags & fl::EXEC != 0)?;
    t.set("is_block", c.flags & fl::BLOCK != 0)?;
    t.set("is_char", c.flags & fl::CHAR != 0)?;
    t.set("is_fifo", c.flags & fl::FIFO != 0)?;
    t.set("is_sock", c.flags & fl::SOCK != 0)?;
    t.set("is_sticky", c.flags & fl::STICKY != 0)?;
    t.set("len", c.len)?;
    t.set("mtime", c.mtime)?;
    t.set("btime", c.btime)?;
    t.set("atime", c.atime)?;
    t.set("mode", c.mode)?;
    t.set("uid", c.uid)?;
    t.set("gid", c.gid)?;
    t.set("nlink", c.nlink)?;
    if let Some(l) = &f.link_to {
        t.set("link_to", l.to_string_lossy().into_owned())?;
    }
    Ok(t)
}

fn raw_folder(lua: &Lua, f: &FolderSnap, all: bool) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("cwd", f.cwd.to_string_lossy().into_owned())?;
    t.set("cursor", f.cursor)?;
    t.set("offset", f.offset)?;
    let files = lua.create_table()?;
    let window = lua.create_table()?;
    for (i, file) in f.files.iter().enumerate() {
        let in_window = f.window.contains(&i);
        if !all && !in_window {
            continue;
        }
        let rf = raw_file(lua, file)?;
        rf.set("is_hovered", f.hovered == Some(i))?;
        if in_window {
            window.push(rf.clone())?;
        }
        files.push(rf)?;
    }
    t.set("files", files)?;
    t.set("window", window)?;
    if let Some(h) = f.hovered.and_then(|i| f.files.get(i)) {
        let rf = raw_file(lua, h)?;
        rf.set("is_hovered", true)?;
        t.set("hovered", rf)?;
    }
    Ok(t)
}

impl Runtime {
    pub fn new(cfg: &Config, theme: &crate::theme::Theme) -> mlua::Result<Self> {
        let lua = Lua::new();
        lua.set_app_data(Shared::default());
        let g = lua.globals();

        // `th`: the theme as plain data; plugin_ui.lua wraps it in ui.Style.
        let th = lua.create_table()?;
        let section = |name: &str| -> mlua::Result<Table> {
            match th.get::<Option<Table>>(name)? {
                Some(t) => Ok(t),
                None => {
                    let t = lua.create_table()?;
                    th.set(name, t.clone())?;
                    Ok(t)
                }
            }
        };
        let (styles, strings) = theme.entries();
        for (k, st) in styles {
            if let Some((sec, key)) = k.split_once('.') {
                section(sec)?.set(key, style_to_lua(&lua, st)?)?;
            }
        }
        for (k, v) in strings {
            if let Some((sec, key)) = k.split_once('.') {
                match key.split_once('.') {
                    None => section(sec)?.set(key, v.clone())?,
                    // `indicator.padding.open` → th.indicator.padding.open
                    Some((sub, leaf)) => {
                        let st = section(sec)?;
                        let t: Table = match st.get::<Option<Table>>(sub)? {
                            Some(t) => t,
                            None => {
                                let t = lua.create_table()?;
                                st.set(sub, t.clone())?;
                                t
                            }
                        };
                        if leaf.parse::<usize>().is_ok() {
                            t.set(leaf.parse::<usize>().unwrap() + 1, v.clone())?;
                        } else {
                            t.set(leaf, v.clone())?;
                        }
                    }
                }
            }
        }
        g.set("__ds_th", th.clone())?;

        let icons_th = theme.clone();
        g.set(
            "__ds_icon",
            lua.create_function(move |lua, (path, name, flags): (String, String, u16)| {
                match icons_th.icons.get(Path::new(&path), &name, flags) {
                    Some(i) => {
                        let t = lua.create_table()?;
                        t.set("text", i.text.clone())?;
                        t.set("style", style_to_lua(lua, &i.style)?)?;
                        Ok(Some(t))
                    }
                    None => Ok(None),
                }
            })?,
        )?;
        let style_th = theme.clone();
        g.set(
            "__ds_file_style",
            lua.create_function(move |lua, (path, mime, flags): (String, String, u16)| {
                style_to_lua(lua, &style_th.file_style(Path::new(&path), &mime, flags))
            })?,
        )?;
        let rules = lua.create_table()?;
        for (k, list) in [
            ("fetchers", &cfg.fetchers),
            ("spotters", &cfg.spotters),
            ("previewers", &cfg.previewers),
        ] {
            let t = lua.create_table()?;
            for r in list {
                let e = lua.create_table()?;
                e.set("run", r.run.clone())?;
                t.push(e)?;
            }
            rules.set(k, t)?;
        }
        g.set("__ds_rules", rules)?;
        g.set(
            "__ds_find_plugin_file",
            lua.create_function(|_, (base, sub): (String, String)| {
                for d in config_dirs() {
                    let p = d
                        .join("plugins")
                        .join(format!("{base}.yazi"))
                        .join(format!("{sub}.lua"));
                    if let Ok(src) = std::fs::read_to_string(&p) {
                        return Ok((Some(src), Some(p.to_string_lossy().into_owned())));
                    }
                }
                Ok((None, None))
            })?,
        )?;
        g.set(
            "__ds_cell_size",
            lua.create_function(|_, ()| {
                Ok(match crossterm::terminal::window_size() {
                    Ok(w) if w.width > 0 && w.columns > 0 && w.rows > 0 => (
                        Some(w.width as f64 / w.columns as f64),
                        Some(w.height as f64 / w.rows as f64),
                    ),
                    _ => (None, None),
                })
            })?,
        )?;
        g.set(
            "__ds_partitions",
            lua.create_function(|lua, ()| {
                let out = Command::new("lsblk")
                    .args(["-J", "-p", "-o", "NAME,MOUNTPOINT,LABEL,FSTYPE,TYPE"])
                    .stderr(Stdio::null())
                    .output();
                let out = match out {
                    Ok(o) if o.status.success() => o,
                    Ok(o) => {
                        return Ok((None, Some(String::from_utf8_lossy(&o.stderr).into_owned())))
                    }
                    Err(e) => return Ok((None, Some(e.to_string()))),
                };
                let v: serde_json::Value =
                    serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
                let list = lua.create_table()?;
                fn walk(lua: &Lua, d: &serde_json::Value, list: &Table) -> mlua::Result<()> {
                    let s = |k: &str| d[k].as_str().map(String::from);
                    if let Some(name) = s("name") {
                        if s("type").as_deref() != Some("loop") {
                            let t = lua.create_table()?;
                            t.set("src", name)?;
                            t.set("dist", s("mountpoint"))?;
                            t.set("label", s("label"))?;
                            t.set("fstype", s("fstype"))?;
                            list.push(t)?;
                        }
                    }
                    for c in d["children"].as_array().into_iter().flatten() {
                        walk(lua, c, list)?;
                    }
                    Ok(())
                }
                for d in v["blockdevices"].as_array().into_iter().flatten() {
                    walk(lua, d, &list)?;
                }
                Ok((Some(list), None))
            })?,
        )?;

        g.set(
            "__ds_width",
            lua.create_function(|_, s: String| Ok(crate::util::width(&s)))?,
        )?;
        g.set(
            "__ds_truncate",
            lua.create_function(|_, (s, max): (String, usize)| {
                Ok(crate::util::truncate(&s, max).to_string())
            })?,
        )?;
        g.set(
            "__ds_truncate_left",
            lua.create_function(|_, (s, max): (String, usize)| {
                let t = crate::util::truncate_left(&s, max + 1);
                Ok(t.trim_start_matches('…').to_string())
            })?,
        )?;
        g.set(
            "__ds_ansi",
            lua.create_function(|lua, s: String| {
                let out = lua.create_table()?;
                for l in crate::preview::ansi_lines(&s) {
                    let lt = lua.create_table()?;
                    for sp in l.spans {
                        let st = style_to_lua(lua, &sp.style)?;
                        st.set("text", sp.content.to_string())?;
                        lt.push(st)?;
                    }
                    out.push(lt)?;
                }
                Ok(out)
            })?,
        )?;
        g.set(
            "__ds_render",
            lua.create_function(|lua, ()| {
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    sh.render = true;
                }
                Ok(())
            })?,
        )?;
        g.set(
            "__ds_components_changed",
            lua.create_function(|lua, ()| {
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    sh.components += 1;
                    sh.render = true;
                }
                Ok(())
            })?,
        )?;
        g.set(
            "__ds_image_info",
            lua.create_function(|_, p: String| {
                let r = image::ImageReader::open(&p)
                    .and_then(|r| r.with_guessed_format())
                    .ok();
                let format = r
                    .as_ref()
                    .and_then(|r| r.format())
                    .map(|f| format!("{f:?}"))
                    .unwrap_or_default();
                match r.and_then(|r| r.into_dimensions().ok()) {
                    Some((w, h)) => Ok((Some(w), Some(h), Some(format))),
                    None => Ok((None, None, None)),
                }
            })?,
        )?;
        g.set(
            "__ds_cache_path",
            lua.create_function(|_, (p, skip): (String, i64)| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                p.hash(&mut h);
                skip.hash(&mut h);
                std::fs::metadata(&p)
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .hash(&mut h);
                let dir = crate::config::cache_dir();
                let _ = std::fs::create_dir_all(&dir);
                Ok(dir
                    .join(format!("plugin-{:016x}", h.finish()))
                    .to_string_lossy()
                    .into_owned())
            })?,
        )?;

        g.set(
            "__ds_emit",
            lua.create_function(|lua, (name, args): (String, Option<Table>)| {
                let mut cmd = Cmd::new(&name);
                if let Some(t) = args {
                    let mut pos: Vec<(i64, String)> = Vec::new();
                    for pair in t.pairs::<Value, Value>() {
                        let (k, v) = pair?;
                        match k {
                            Value::Integer(i) => pos.push((i, val_to_string(lua, &v))),
                            Value::String(k) => {
                                let k = k.to_string_lossy().replace('_', "-");
                                if matches!(v, Value::Boolean(false)) {
                                    continue;
                                }
                                cmd.named.push((k, val_to_string(lua, &v)));
                            }
                            _ => {}
                        }
                    }
                    pos.sort_by_key(|p| p.0);
                    cmd.args = pos.into_iter().map(|p| p.1).collect();
                }
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    sh.emits.push(cmd);
                }
                Ok(())
            })?,
        )?;

        g.set(
            "__ds_notify",
            lua.create_function(|lua, t: Table| {
                let title: String = t.get::<Option<String>>("title")?.unwrap_or_default();
                let content: String = t
                    .get::<Option<String>>("content")?
                    .or(t.get::<Option<String>>("body")?)
                    .unwrap_or_default();
                let level = match t.get::<Option<String>>("level")?.as_deref() {
                    Some("warn") => Level::Warn,
                    Some("error") => Level::Error,
                    _ => Level::Info,
                };
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    sh.notes.push((level, title, content));
                }
                Ok(())
            })?,
        )?;

        g.set(
            "__ds_cmd_output",
            lua.create_function(|lua, spec: Table| {
                let hidden = lua.app_data_ref::<Shared>().is_some_and(|s| s.hidden);
                let mut c = build_command(&spec, hidden, true)?;
                match c.output() {
                    Ok(o) => {
                        let t = lua.create_table()?;
                        t.set("stdout", lua.create_string(&o.stdout)?)?;
                        t.set("stderr", lua.create_string(&o.stderr)?)?;
                        t.set("status", status_table(lua, o.status)?)?;
                        Ok((Some(t), None))
                    }
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_cmd_status",
            lua.create_function(|lua, spec: Table| {
                let hidden = lua.app_data_ref::<Shared>().is_some_and(|s| s.hidden);
                let mut c = build_command(&spec, hidden, false)?;
                match c.status() {
                    Ok(st) => Ok((Some(status_table(lua, st)?), None)),
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_cmd_spawn",
            lua.create_function(|lua, spec: Table| {
                let hidden = lua.app_data_ref::<Shared>().is_some_and(|s| s.hidden);
                let mut c = build_command(&spec, hidden, false)?;
                match c.spawn() {
                    Ok(mut child) => {
                        let out = child.stdout.take().map(BufReader::new);
                        let err = child.stderr.take().map(BufReader::new);
                        let mut sh = lua
                            .app_data_mut::<Shared>()
                            .ok_or_else(|| mlua::Error::runtime("no state"))?;
                        sh.next_proc += 1;
                        let id = sh.next_proc;
                        sh.procs.insert(id, Proc { child, out, err });
                        Ok((Some(id), None))
                    }
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_child_read_line",
            lua.create_function(|lua, id: i64| {
                let mut sh = lua
                    .app_data_mut::<Shared>()
                    .ok_or_else(|| mlua::Error::runtime("no state"))?;
                let Some(p) = sh.procs.get_mut(&id) else {
                    return Ok((String::new(), 2));
                };
                let mut line = String::new();
                if let Some(o) = p.out.as_mut() {
                    if o.read_line(&mut line).unwrap_or(0) > 0 {
                        return Ok((line, 0));
                    }
                    p.out = None;
                }
                if let Some(e) = p.err.as_mut() {
                    if e.read_line(&mut line).unwrap_or(0) > 0 {
                        return Ok((line, 1));
                    }
                    p.err = None;
                }
                Ok((String::new(), 2))
            })?,
        )?;

        g.set(
            "__ds_child_write",
            lua.create_function(|lua, (id, data): (i64, LuaString)| {
                let mut sh = lua
                    .app_data_mut::<Shared>()
                    .ok_or_else(|| mlua::Error::runtime("no state"))?;
                let ok = sh
                    .procs
                    .get_mut(&id)
                    .and_then(|p| p.child.stdin.as_mut())
                    .map(|i| i.write_all(&data.as_bytes()).is_ok())
                    .unwrap_or(false);
                Ok(ok)
            })?,
        )?;

        g.set(
            "__ds_child_close_stdin",
            lua.create_function(|lua, id: i64| {
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    if let Some(p) = sh.procs.get_mut(&id) {
                        p.child.stdin.take();
                    }
                }
                Ok(())
            })?,
        )?;

        g.set(
            "__ds_child_wait",
            lua.create_function(|lua, (id, collect): (i64, bool)| {
                let p = lua
                    .app_data_mut::<Shared>()
                    .and_then(|mut s| s.procs.remove(&id));
                let Some(mut p) = p else {
                    return Ok((None, Some("no such child".to_string())));
                };
                p.child.stdin.take();
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut o) = p.out.take() {
                    let _ = o.read_to_end(&mut out);
                }
                if let Some(mut e) = p.err.take() {
                    let _ = e.read_to_end(&mut err);
                }
                match p.child.wait() {
                    Ok(st) => {
                        let status = status_table(lua, st)?;
                        if !collect {
                            return Ok((Some(status), None));
                        }
                        let t = lua.create_table()?;
                        t.set("stdout", lua.create_string(&out)?)?;
                        t.set("stderr", lua.create_string(&err)?)?;
                        t.set("status", status)?;
                        Ok((Some(t), None))
                    }
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_child_kill",
            lua.create_function(|lua, id: i64| {
                if let Some(mut sh) = lua.app_data_mut::<Shared>() {
                    if let Some(p) = sh.procs.get_mut(&id) {
                        return Ok(p.child.kill().is_ok());
                    }
                }
                Ok(false)
            })?,
        )?;

        g.set(
            "__ds_fs_cha",
            lua.create_function(|lua, (path, follow): (String, Option<bool>)| {
                let p = Path::new(&path);
                let r = if follow.unwrap_or(false) {
                    std::fs::metadata(p)
                        .map(|_| ())
                        .and_then(|_| File::from_path(p))
                } else {
                    File::from_path(p)
                };
                match r {
                    Ok(f) => Ok((Some(raw_file(lua, &f)?), None)),
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_fs_read_dir",
            lua.create_function(|lua, (path, limit): (String, Option<usize>)| {
                match crate::fs::read_dir(Path::new(&path)) {
                    Ok(mut files) => {
                        files.sort_by(|a, b| crate::util::natural_cmp(&a.name, &b.name, false));
                        let t = lua.create_table()?;
                        for f in files
                            .iter()
                            .take(limit.filter(|l| *l > 0).unwrap_or(usize::MAX))
                        {
                            t.push(raw_file(lua, f)?)?;
                        }
                        Ok((Some(t), None))
                    }
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;

        g.set(
            "__ds_fs_write",
            lua.create_function(|_, (path, data): (String, LuaString)| {
                Ok(match std::fs::write(&path, data.as_bytes()) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                })
            })?,
        )?;

        g.set(
            "__ds_fs_read",
            lua.create_function(|lua, path: String| match std::fs::read(&path) {
                Ok(d) => Ok((Some(lua.create_string(&d)?), None)),
                Err(e) => Ok((None, Some(e.to_string()))),
            })?,
        )?;

        g.set(
            "__ds_fs_remove",
            lua.create_function(|_, (kind, path): (String, String)| {
                let p = Path::new(&path);
                let r = match kind.as_str() {
                    "file" => std::fs::remove_file(p),
                    "dir" => std::fs::remove_dir(p),
                    "dir_all" => std::fs::remove_dir_all(p),
                    "dir_clean" => {
                        // Remove empty directories under (and including) p.
                        fn clean(p: &Path) {
                            if let Ok(rd) = std::fs::read_dir(p) {
                                for e in rd.flatten() {
                                    if e.file_type().is_ok_and(|t| t.is_dir()) {
                                        clean(&e.path());
                                    }
                                }
                            }
                            let _ = std::fs::remove_dir(p);
                        }
                        clean(p);
                        Ok(())
                    }
                    other => Err(std::io::Error::other(format!("unknown kind {other}"))),
                };
                Ok(match r {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                })
            })?,
        )?;

        g.set(
            "__ds_fs_create",
            lua.create_function(|_, (kind, path): (String, String)| {
                let r = match kind.as_str() {
                    "dir" => std::fs::create_dir(&path),
                    _ => std::fs::create_dir_all(&path),
                };
                Ok(match r {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                })
            })?,
        )?;

        g.set(
            "__ds_fs_rename",
            lua.create_function(|_, (a, b): (String, String)| {
                Ok(match std::fs::rename(&a, &b) {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                })
            })?,
        )?;
        g.set(
            "__ds_fs_copy",
            lua.create_function(|_, (a, b): (String, String)| {
                Ok(match std::fs::copy(&a, &b) {
                    Ok(n) => (Some(n), None),
                    Err(e) => (None, Some(e.to_string())),
                })
            })?,
        )?;
        g.set(
            "__ds_json_decode",
            lua.create_function(|lua, s: String| {
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(v) => Ok((Some(lua.to_value(&v)?), None)),
                    Err(e) => Ok((None, Some(e.to_string()))),
                }
            })?,
        )?;
        g.set(
            "__ds_json_encode",
            lua.create_function(|lua, v: Value| {
                let j: serde_json::Value = lua.from_value(v)?;
                Ok(j.to_string())
            })?,
        )?;
        g.set(
            "__ds_fs_unique",
            lua.create_function(|_, path: String| {
                Ok(crate::util::unique_name(Path::new(&path))
                    .to_string_lossy()
                    .into_owned())
            })?,
        )?;

        g.set(
            "__ds_cwd",
            lua.create_function(|_, ()| {
                Ok(std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default())
            })?,
        )?;

        g.set(
            "__ds_clipboard",
            lua.create_function(|_, text: Option<String>| {
                if let Some(t) = text {
                    let _ = crate::clip::copy(&t);
                    return Ok(None);
                }
                let out = Command::new("wl-paste")
                    .arg("--no-newline")
                    .stderr(Stdio::null())
                    .output()
                    .or_else(|_| {
                        Command::new("xclip")
                            .args(["-selection", "clipboard", "-o"])
                            .stderr(Stdio::null())
                            .output()
                    });
                Ok(out
                    .ok()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned()))
            })?,
        )?;

        g.set(
            "__ds_log",
            lua.create_function(|_, (level, msg): (String, String)| {
                if let Ok(dir) = crate::config::state_dir() {
                    if let Ok(mut f) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(dir.join("plugins.log"))
                    {
                        let _ = writeln!(f, "[{level}] {msg}");
                    }
                }
                Ok(())
            })?,
        )?;

        g.set(
            "__ds_sleep",
            lua.create_function(|_, secs: f64| {
                std::thread::sleep(std::time::Duration::from_secs_f64(secs.clamp(0.0, 60.0)));
                Ok(())
            })?,
        )?;

        g.set(
            "__ds_time",
            lua.create_function(|_, ()| {
                Ok(std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0))
            })?,
        )?;

        g.set(
            "__ds_hash",
            lua.create_function(|_, v: LuaString| {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                v.as_bytes().hash(&mut h);
                Ok(format!("{:016x}", h.finish()))
            })?,
        )?;

        g.set(
            "__ds_quote",
            lua.create_function(|_, v: String| Ok(crate::util::shell_quote(&v)))?,
        )?;
        g.set(
            "__ds_readable_size",
            lua.create_function(|_, v: f64| Ok(crate::util::readable_size(v.max(0.0) as u64)))?,
        )?;
        g.set(
            "__ds_readable_path",
            lua.create_function(|_, v: String| Ok(crate::util::readable_path(Path::new(&v))))?,
        )?;
        g.set(
            "__ds_perm",
            lua.create_function(|_, v: u32| Ok(crate::util::perm_string(v)))?,
        )?;
        g.set(
            "__ds_user_name",
            lua.create_function(|_, v: Option<u32>| {
                // SAFETY: getuid has no preconditions.
                Ok(crate::util::user_name(
                    v.unwrap_or_else(|| unsafe { libc::getuid() }),
                ))
            })?,
        )?;
        g.set(
            "__ds_group_name",
            lua.create_function(|_, v: Option<u32>| {
                // SAFETY: getgid has no preconditions.
                Ok(crate::util::group_name(
                    v.unwrap_or_else(|| unsafe { libc::getgid() }),
                ))
            })?,
        )?;
        g.set(
            "__ds_ids",
            lua.create_function(|_, ()| {
                // SAFETY: getuid/getgid have no preconditions.
                Ok(unsafe { (libc::getuid(), libc::getgid()) })
            })?,
        )?;
        g.set(
            "__ds_host_name",
            lua.create_function(|_, ()| {
                Ok(std::fs::read_to_string("/etc/hostname")
                    .map(|s| s.trim().to_string())
                    .or_else(|_| std::env::var("HOSTNAME"))
                    .unwrap_or_default())
            })?,
        )?;
        g.set(
            "__ds_find_plugin",
            lua.create_function(|_, name: String| {
                let found = find_plugin(&name).and_then(|p| {
                    std::fs::read_to_string(&p)
                        .ok()
                        .map(|src| (src, p.to_string_lossy().into_owned()))
                });
                Ok(match found {
                    Some((src, path)) => (Some(src), Some(path)),
                    None => (None, None),
                })
            })?,
        )?;

        // `rt`: the runtime config plugins read.
        let rt = lua.create_table()?;
        let mgr = lua.create_table()?;
        mgr.set("show_hidden", cfg.mgr.show_hidden)?;
        mgr.set("show_symlink", cfg.mgr.show_symlink)?;
        mgr.set("scrolloff", cfg.mgr.scrolloff)?;
        mgr.set("linemode", cfg.mgr.linemode.clone())?;
        let ratio = lua.create_table()?;
        ratio.set("parent", cfg.mgr.ratio[0])?;
        ratio.set("current", cfg.mgr.ratio[1])?;
        ratio.set("preview", cfg.mgr.ratio[2])?;
        ratio.set(
            "all",
            cfg.mgr.ratio[0] + cfg.mgr.ratio[1] + cfg.mgr.ratio[2],
        )?;
        mgr.set("ratio", ratio)?;
        rt.set("mgr", mgr.clone())?;
        rt.set("manager", mgr)?;
        let preview = lua.create_table()?;
        preview.set("tab_size", cfg.preview.tab_size)?;
        preview.set("max_width", cfg.preview.max_width)?;
        preview.set("max_height", cfg.preview.max_height)?;
        preview.set("image_delay", 30)?;
        preview.set("image_quality", cfg.preview.image_quality)?;
        preview.set("image_filter", "triangle")?;
        preview.set("wrap", if cfg.preview.wrap { "yes" } else { "no" })?;
        preview.set("cache_dir", s(crate::config::cache_dir().display()))?;
        rt.set("preview", preview)?;
        g.set("__ds_rt", rt)?;

        lua.load(PRELUDE).set_name("@wisp/prelude.lua").exec()?;
        lua.load(UI).set_name("@wisp/ui.lua").exec()?;
        Ok(Self {
            lua,
            running: None,
            hidden: false,
            active: std::cell::RefCell::new((u64::MAX, HashMap::new())),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// This state runs previewers on the preview thread.
    pub fn mark_preview_thread(&self) {
        let _ = self.lua.globals().set("__ds_preview_thread", true);
    }

    /// `ui.render()` was requested since the last call.
    pub fn take_render(&self) -> bool {
        self.lua
            .app_data_mut::<Shared>()
            .map(|mut s| std::mem::take(&mut s.render))
            .unwrap_or(false)
    }

    /// Whether a component has Lua children or overrides (cached).
    pub fn component_active(&self, name: &str) -> bool {
        let gen = self
            .lua
            .app_data_ref::<Shared>()
            .map(|s| s.components)
            .unwrap_or(0);
        let mut a = self.active.borrow_mut();
        if a.0 != gen {
            a.0 = gen;
            a.1.clear();
        }
        if let Some(v) = a.1.get(name) {
            return *v;
        }
        let v = self
            .lua
            .globals()
            .get::<Function>("__ds_component_active")
            .and_then(|f| f.call::<bool>(name))
            .unwrap_or(false);
        a.1.insert(name.to_string(), v);
        v
    }

    fn ctx_value(
        &self,
        ctx: Option<&FileCtx>,
        area: Option<ratatui::layout::Rect>,
    ) -> mlua::Result<Value> {
        if let Some(c) = ctx {
            let t = raw_file(&self.lua, c.file)?;
            t.set("is_hovered", c.hovered)?;
            t.set("in_current", c.in_current)?;
            t.set("in_preview", c.in_preview)?;
            t.set("in_parent", c.in_parent)?;
            return Ok(Value::Table(t));
        }
        let t = self.lua.create_table()?;
        if let Some(a) = area {
            t.set("x", a.x)?;
            t.set("y", a.y)?;
            t.set("w", a.width)?;
            t.set("h", a.height)?;
        }
        Ok(Value::Table(t))
    }

    /// A user override of a built-in slot (e.g. `function Status:name()`).
    pub fn component_override(
        &self,
        name: &str,
        method: &str,
        ctx: Option<&FileCtx>,
        area: Option<ratatui::layout::Rect>,
    ) -> Option<Spans> {
        let kind = if ctx.is_some() { "file" } else { "area" };
        let raw = self.ctx_value(ctx, area).ok()?;
        let f: Function = self.lua.globals().get("__ds_component_override").ok()?;
        let v = f.call::<Value>((name, method, kind, raw)).ok()?;
        if v.is_nil() {
            return None;
        }
        let mut out = Vec::new();
        widget_spans(&v, Style::default(), &mut out);
        Some(out)
    }

    /// Registered children of a component, as `(order, spans)`.
    pub fn component_children(
        &self,
        name: &str,
        ctx: Option<&FileCtx>,
        area: Option<ratatui::layout::Rect>,
        side: Option<i64>,
    ) -> Vec<(i64, Spans)> {
        let kind = if ctx.is_some() { "file" } else { "area" };
        let Ok(raw) = self.ctx_value(ctx, area) else {
            return Vec::new();
        };
        let Ok(f) = self
            .lua
            .globals()
            .get::<Function>("__ds_component_children")
        else {
            return Vec::new();
        };
        let Ok(Value::Table(list)) = f.call::<Value>((name, kind, raw, side)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in list.sequence_values::<Table>().flatten() {
            let order: i64 = item.get(1).unwrap_or(9999);
            let w: Value = item.get(2).unwrap_or(Value::Nil);
            let mut spans = Vec::new();
            widget_spans(&w, Style::default(), &mut spans);
            if !spans.is_empty() {
                out.push((order, spans));
            }
        }
        out
    }

    /// Whether component `name` has method `method` (e.g. a custom linemode).
    /// Whether a plugin overrode `Current`/`Parent`'s `redraw`.
    pub fn list_custom(&self, name: &str) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_list_custom")
            .and_then(|f| f.call::<bool>(name))
            .unwrap_or(false)
    }

    /// Run a list component's (overridden) redraw for `area`.
    pub fn list_redraw(&self, name: &str, area: ratatui::layout::Rect) -> Vec<Draw> {
        let Ok(f) = self.lua.globals().get::<Function>("__ds_list_redraw") else {
            return Vec::new();
        };
        match f.call::<Value>((name, area.x, area.y, area.width, area.height)) {
            Ok(v) => widget_draws(&v),
            Err(_) => Vec::new(),
        }
    }

    /// Whether a plugin overrode `Root.layout`.
    pub fn root_custom(&self) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_root_custom")
            .and_then(|f| f.call::<bool>(()))
            .unwrap_or(false)
    }

    /// The (overridden) Root layout: header, tabs, tab, status.
    pub fn root_layout(&self, w: u16, h: u16, tabs: u16) -> Option<[ratatui::layout::Rect; 4]> {
        let f: Function = self.lua.globals().get("__ds_root_layout").ok()?;
        let t: Table = f.call((w, h, tabs)).ok()?;
        let r: Vec<ratatui::layout::Rect> = t
            .sequence_values::<Value>()
            .flatten()
            .filter_map(|v| lua_rect(&v))
            .collect();
        (r.len() >= 4).then(|| [r[0], r[1], r[2], r[3]])
    }

    /// Whether a plugin UI is registered as a `Modal` child.
    pub fn modal_active(&self) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_modal_active")
            .and_then(|f| f.call::<bool>(()))
            .unwrap_or(false)
    }

    /// What `Modal` children draw on a `w`×`h` screen.
    pub fn modal_draws(&self, w: u16, h: u16) -> Vec<Draw> {
        let Ok(f) = self.lua.globals().get::<Function>("__ds_modal_draw") else {
            return Vec::new();
        };
        match f.call::<Value>((w, h)) {
            Ok(v) => widget_draws(&v),
            Err(_) => Vec::new(),
        }
    }

    /// Whether a plugin replaced `Tab.layout`/`Tab.build` (full-border…).
    pub fn tab_custom(&self) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_tab_custom")
            .and_then(|f| f.call::<bool>(()))
            .unwrap_or(false)
    }

    /// Whether a plugin hid the status bar (`Status.redraw` → `{}`).
    pub fn status_hidden(&self) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_status_hidden")
            .and_then(|f| f.call::<bool>(()))
            .unwrap_or(false)
    }

    /// Run the (plugin-wrapped) Tab layout for `area`.
    pub fn tab_layout(&self, area: ratatui::layout::Rect) -> Option<TabLayout> {
        let f: Function = self.lua.globals().get("__ds_tab_layout").ok()?;
        let out: Table = f.call((area.x, area.y, area.width, area.height)).ok()?;
        let rect = |t: &Table| -> Option<ratatui::layout::Rect> {
            let n = |k: &str| {
                t.get::<Option<f64>>(k)
                    .ok()
                    .flatten()
                    .map(|v| v.max(0.0) as u16)
            };
            Some(ratatui::layout::Rect::new(
                n("x")?,
                n("y")?,
                n("w")?,
                n("h")?,
            ))
        };
        let chunks_t: Table = out.get("chunks").ok()?;
        let chunks: Vec<ratatui::layout::Rect> = chunks_t
            .sequence_values::<Table>()
            .flatten()
            .filter_map(|t| rect(&t))
            .collect();
        if chunks.len() != 3 {
            return None;
        }
        let mut borders = Vec::new();
        if let Ok(bs) = out.get::<Table>("borders") {
            for b in bs.sequence_values::<Table>().flatten() {
                let Some(r) = b.get::<Table>("area").ok().and_then(|t| rect(&t)) else {
                    continue;
                };
                let kind: i64 = b.get("type").unwrap_or(0);
                let style = lua_style(&b.get::<Value>("style").unwrap_or(Value::Nil));
                borders.push((r, kind, style, b.get::<bool>("merge").unwrap_or(false)));
            }
        }
        Some(TabLayout {
            chunks: [chunks[0], chunks[1], chunks[2]],
            borders,
        })
    }

    #[cfg(test)]
    pub fn has_method(&self, name: &str, method: &str) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_has_method")
            .and_then(|f| f.call::<bool>((name, method)))
            .unwrap_or(false)
    }

    /// Make `cx` reflect `snap` (before rendering Lua components).
    pub fn refresh_cx(&self, snap: &Snapshot) {
        let _ = self.set_cx(snap);
    }

    /// Run a Lua previewer's `peek`.
    #[allow(clippy::too_many_arguments)]
    pub fn peek(
        &self,
        name: &str,
        file: &File,
        mime: &str,
        skip: usize,
        w: u16,
        h: u16,
        args: &str,
    ) -> PeekOut {
        let r = (|| -> mlua::Result<PeekOut> {
            let raw = raw_file(&self.lua, file)?;
            // yazi's previewer args: `--key=value` options, positional words,
            // and everything after ` -- ` as one positional argument.
            let a = self.lua.create_table()?;
            let args = args.trim();
            let (head, tail) = if let Some(rest) = args.strip_prefix("-- ") {
                ("", Some(rest))
            } else if let Some((h, t)) = args.split_once(" -- ") {
                (h, Some(t))
            } else {
                (args, None)
            };
            let mut n = 0;
            for w in crate::util::split_words(head) {
                match w.strip_prefix("--") {
                    Some(kv) => match kv.split_once('=') {
                        Some((k, v)) => a.set(k.replace('-', "_"), v)?,
                        None => a.set(kv.replace('-', "_"), true)?,
                    },
                    None => {
                        n += 1;
                        a.set(n, w)?;
                    }
                }
            }
            if let Some(t) = tail {
                a.set(n + 1, t)?;
            }
            let f: Function = self.lua.globals().get("__ds_peek")?;
            let out: Table = f.call((name, raw, mime, skip, w, h, a))?;
            let mut p = PeekOut {
                error: out.get("error")?,
                native: out.get("native")?,
                image: out.get::<Option<String>>("image")?.map(PathBuf::from),
                peek: out.get::<Option<f64>>("peek")?.map(|n| n.max(0.0) as usize),
                lines: None,
            };
            let wv: Value = out.get("widgets")?;
            if !wv.is_nil() {
                p.lines = Some(widget_lines(&wv, w as usize));
            }
            Ok(p)
        })();
        r.unwrap_or_else(|e| PeekOut {
            error: Some(e.to_string()),
            ..Default::default()
        })
    }

    /// Run a Lua spotter: rows from `ya.spot_table`, or `None` to use the
    /// built-in spot.
    pub fn spot(&self, name: &str, file: &File, mime: &str) -> SpotResult {
        let r = (|| -> mlua::Result<SpotResult> {
            let raw = raw_file(&self.lua, file)?;
            let f: Function = self.lua.globals().get("__ds_spot")?;
            let out: Table = f.call((name, raw, mime))?;
            if let Some(e) = out.get::<Option<String>>("error")? {
                return Ok(Err(e));
            }
            let Ok(t) = out.get::<Table>("table") else {
                return Ok(Ok(None));
            };
            let mut rows = Vec::new();
            for r in t.get::<Table>("rows")?.sequence_values::<Table>().flatten() {
                let cells: Vec<String> = r
                    .get::<Table>("cells")
                    .map(|c| {
                        c.sequence_values::<Value>()
                            .flatten()
                            .map(|v| {
                                let v = match &v {
                                    Value::Table(t) if tag(t).is_empty() && t.raw_len() > 0 => t
                                        .sequence_values::<Value>()
                                        .flatten()
                                        .next()
                                        .unwrap_or(Value::Nil),
                                    _ => v,
                                };
                                let mut s = Vec::new();
                                widget_spans(&v, Style::default(), &mut s);
                                s.into_iter().map(|x| x.0).collect::<String>()
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                match cells.len() {
                    0 => rows.push((String::new(), String::new())),
                    1 => rows.push((cells[0].clone(), String::new())),
                    _ => rows.push((cells[0].clone(), cells[1..].join(" "))),
                }
            }
            Ok(Ok(Some(rows)))
        })();
        r.unwrap_or_else(|e| Err(e.to_string()))
    }

    /// Run a Lua fetcher over `files`; returns an error message on failure.
    pub fn fetch(&self, name: &str, files: &[File]) -> Option<String> {
        let r = (|| -> mlua::Result<Option<String>> {
            let list = self.lua.create_table()?;
            for f in files {
                list.push(raw_file(&self.lua, f)?)?;
            }
            let f: Function = self.lua.globals().get("__ds_fetch")?;
            f.call::<Option<String>>((name, list))
        })();
        r.unwrap_or_else(|e| Some(e.to_string()))
    }

    /// Run Lua source as if it were `init.lua`.
    #[cfg(test)]
    pub fn run_init_src(&self, src: &str) -> Result<(), String> {
        self.lua
            .load(src)
            .set_name("@init.lua")
            .exec()
            .map_err(|e| e.to_string())
    }

    /// Run the user's `init.lua`.
    pub fn run_init(&mut self, path: &Path) -> Result<(), String> {
        let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        self.lua
            .load(&src)
            .set_name(format!("@{}", path.display()))
            .exec()
            .map_err(|e| e.to_string())
    }

    fn set_cx(&self, snap: &Snapshot) -> mlua::Result<()> {
        let lua = &self.lua;
        let raw = lua.create_table()?;
        let tabs = lua.create_table()?;
        for (i, t) in snap.tabs.iter().enumerate() {
            let rt = lua.create_table()?;
            rt.set("name", t.name.clone())?;
            rt.set("cwd", t.cwd.to_string_lossy().into_owned())?;
            rt.set("current", raw_folder(lua, &t.current, i == snap.cur)?)?;
            if let Some(p) = &t.parent {
                rt.set("parent", raw_folder(lua, p, false)?)?;
            }
            let pv = lua.create_table()?;
            pv.set("skip", t.preview_skip)?;
            if let Some(f) = &t.preview {
                pv.set("folder", raw_folder(lua, f, false)?)?;
            }
            rt.set("preview", pv)?;
            let sel = lua.create_table()?;
            for p in &t.selected {
                sel.push(p.to_string_lossy().into_owned())?;
            }
            rt.set("selected", sel)?;
            rt.set("mode", t.mode)?;
            rt.set("visual_start", t.visual_start)?;
            let pref = lua.create_table()?;
            pref.set("sort_by", t.sort_by.clone())?;
            pref.set("sort_reverse", t.sort_reverse)?;
            pref.set("sort_dir_first", t.sort_dir_first)?;
            pref.set("sort_sensitive", t.sort_sensitive)?;
            pref.set("linemode", t.linemode.clone())?;
            pref.set("show_hidden", t.show_hidden)?;
            rt.set("pref", pref)?;
            tabs.push(rt)?;
        }
        raw.set("tabs", tabs)?;
        raw.set("cur", snap.cur + 1)?;
        let y = lua.create_table()?;
        for p in &snap.yanked {
            y.push(p.to_string_lossy().into_owned())?;
        }
        raw.set("yanked", y)?;
        raw.set("cut", snap.cut)?;
        let areas = lua.create_table()?;
        for (name, a) in &snap.areas {
            let t = lua.create_table()?;
            t.set("x", a.x)?;
            t.set("y", a.y)?;
            t.set("w", a.width)?;
            t.set("h", a.height)?;
            areas.set(*name, t)?;
        }
        lua.globals().set("__ds_areas", areas)?;
        let wrap: Function = lua.globals().get("__ds_wrap_cx")?;
        wrap.call::<()>(raw)
    }

    fn take_shared(&self) -> (Vec<Cmd>, Vec<(Level, String, String)>) {
        match self.lua.app_data_mut::<Shared>() {
            Some(mut sh) => (std::mem::take(&mut sh.emits), std::mem::take(&mut sh.notes)),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Commands and notifications queued by the last step.
    pub fn drain(&self) -> (Vec<Cmd>, Vec<(Level, String, String)>) {
        self.take_shared()
    }

    pub fn set_hidden(&mut self, h: bool) {
        self.hidden = h;
        if let Some(mut sh) = self.lua.app_data_mut::<Shared>() {
            sh.hidden = h;
        }
    }

    /// Start `plugin name args...`.
    pub fn start(&mut self, name: &str, cmd: &Cmd, snap: &Snapshot) -> Step {
        if self.running.is_some() {
            return Step::Error("another plugin is still running".into());
        }
        let r = (|| -> mlua::Result<Step> {
            self.set_cx(snap)?;
            let job = self.lua.create_table()?;
            let args = self.lua.create_table()?;
            for (i, a) in cmd.args.iter().skip(1).enumerate() {
                args.set(i + 1, a.clone())?;
                job.set(i + 1, a.clone())?;
            }
            for (k, v) in &cmd.named {
                if k == "args" {
                    for (i, w) in crate::util::split_words(v).into_iter().enumerate() {
                        args.set(i + 1, w.clone())?;
                        job.set(i + 1, w)?;
                    }
                    continue;
                }
                let val: Value = if v == "yes" {
                    Value::Boolean(true)
                } else {
                    Value::String(self.lua.create_string(v)?)
                };
                args.set(k.replace('-', "_"), val)?;
            }
            job.set("args", args)?;
            let run: Function = self.lua.globals().get("__ds_run")?;
            let th = self.lua.create_thread(run)?;
            self.running = Some((name.to_string(), th));
            self.step(MultiValue::from_vec(vec![
                Value::String(self.lua.create_string(name)?),
                Value::Table(job),
            ]))
        })();
        r.unwrap_or_else(|e| {
            self.running = None;
            Step::Error(e.to_string())
        })
    }

    /// Resume a suspended plugin.
    pub fn resume(&mut self, how: Resume, snap: &Snapshot) -> Step {
        let r = (|| -> mlua::Result<Step> {
            self.set_cx(snap)?;
            let args = match how {
                Resume::Nothing => MultiValue::new(),
                Resume::Input(Some(v)) => MultiValue::from_vec(vec![
                    Value::String(self.lua.create_string(&v)?),
                    Value::Integer(1),
                ]),
                Resume::Input(None) => MultiValue::from_vec(vec![Value::Nil, Value::Integer(2)]),
                Resume::Changed(v) => MultiValue::from_vec(vec![
                    Value::String(self.lua.create_string(&v)?),
                    Value::Integer(3),
                ]),
                Resume::Bool(b) => MultiValue::from_vec(vec![Value::Boolean(b)]),
                Resume::Index(Some(i)) => MultiValue::from_vec(vec![Value::Integer(i as i64)]),
                Resume::Index(None) => MultiValue::new(),
            };
            self.step(args)
        })();
        r.unwrap_or_else(|e| {
            self.running = None;
            Step::Error(e.to_string())
        })
    }

    fn step(&mut self, args: MultiValue) -> mlua::Result<Step> {
        let Some((name, th)) = self.running.clone() else {
            return Ok(Step::Done);
        };
        let out = th.resume::<Value>(args);
        let out = match out {
            Ok(v) => v,
            Err(e) => {
                self.running = None;
                return Ok(Step::Error(format!("{name}: {e}")));
            }
        };
        if th.status() != ThreadStatus::Resumable {
            self.running = None;
            return Ok(Step::Done);
        }
        let Value::Table(op) = out else {
            // A bare `coroutine.yield()`: keep going.
            return self.step(MultiValue::new());
        };
        let kind: String = op.get::<Option<String>>("op")?.unwrap_or_default();
        Ok(match kind.as_str() {
            "input" => Step::Input {
                title: op.get::<Option<String>>("title")?.unwrap_or_default(),
                value: op.get::<Option<String>>("value")?.unwrap_or_default(),
                obscure: op.get::<Option<bool>>("obscure")?.unwrap_or(false),
            },
            "input_recv" => Step::InputRecv {
                title: op.get::<Option<String>>("title")?.unwrap_or_default(),
                value: op.get::<Option<String>>("value")?.unwrap_or_default(),
                obscure: op.get::<Option<bool>>("obscure")?.unwrap_or(false),
                open: op.get::<Option<bool>>("open")?.unwrap_or(false),
            },
            "confirm" => Step::Confirm {
                title: op.get::<Option<String>>("title")?.unwrap_or_default(),
                body: op.get::<Option<String>>("body")?.unwrap_or_default(),
            },
            "which" => {
                let mut cands = Vec::new();
                if let Ok(t) = op.get::<Table>("cands") {
                    for c in t.sequence_values::<Table>() {
                        let c = c?;
                        let keys: Vec<Key> = match c.get::<Value>("on")? {
                            Value::String(s) => {
                                Key::parse(&s.to_string_lossy()).into_iter().collect()
                            }
                            Value::Table(t) => t
                                .sequence_values::<String>()
                                .filter_map(|k| k.ok().and_then(|k| Key::parse(&k)))
                                .collect(),
                            _ => Vec::new(),
                        };
                        let desc = c.get::<Option<String>>("desc")?.unwrap_or_default();
                        cands.push((keys, desc));
                    }
                }
                let silent = op.get::<Option<bool>>("silent")?.unwrap_or(false);
                Step::Which(cands, silent)
            }
            "hide" => Step::Hide,
            "show" => Step::Show,
            other => Step::Error(format!("{name}: unknown request `{other}`")),
        })
    }

    /// The pane ratio if a plugin changed `rt.mgr.ratio` (toggle-pane etc.).
    pub fn ratio(&self) -> Option<[u16; 3]> {
        let rt: Table = self.lua.globals().get("rt").ok()?;
        let mgr: Table = rt.get("mgr").ok()?;
        let r: Table = mgr.get("ratio").ok()?;
        let n = |i: i64| -> Option<u16> {
            r.get::<Option<f64>>(i)
                .ok()
                .flatten()
                .map(|v| v.clamp(0.0, 9999.0) as u16)
        };
        Some([n(1)?, n(2)?, n(3)?])
    }

    /// Whether any plugin subscribed to `kind` via `ps.sub`.
    pub fn has_sub(&self, kind: &str) -> bool {
        self.lua
            .globals()
            .get::<Function>("__ds_has_sub")
            .and_then(|f| f.call::<bool>(kind))
            .unwrap_or(false)
    }

    /// Deliver a `ps` event to subscribers.
    pub fn fire(&mut self, kind: &str, snap: &Snapshot) -> Option<String> {
        let r = (|| -> mlua::Result<()> {
            self.set_cx(snap)?;
            let f: Function = self.lua.globals().get("__ds_fire")?;
            f.call::<()>((kind, Value::Nil))
        })();
        r.err().map(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(dir: &Path) -> Snapshot {
        let files = crate::fs::read_dir(dir).unwrap();
        let n = files.len();
        Snapshot {
            areas: vec![],
            tabs: vec![TabSnap {
                name: "t".into(),
                cwd: dir.to_path_buf(),
                current: FolderSnap {
                    cwd: dir.to_path_buf(),
                    files,
                    cursor: 0,
                    offset: 0,
                    window: 0..n,
                    hovered: Some(0),
                },
                parent: None,
                preview: None,
                preview_skip: 0,
                selected: vec![],
                mode: "normal",
                visual_start: 0,
                sort_by: "natural".into(),
                sort_reverse: false,
                sort_dir_first: true,
                sort_sensitive: false,
                linemode: "none".into(),
                show_hidden: false,
            }],
            cur: 0,
            yanked: vec![],
            cut: false,
        }
    }

    #[test]
    fn ui_components_previewers_spotters_fetchers() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("cfg");
        let pv = cfg.join("plugins/glowish.yazi");
        std::fs::create_dir_all(&pv).unwrap();
        std::fs::write(
            pv.join("main.lua"),
            r#"
return {
  peek = function(self, job)
    local lines = {}
    for i = 1 + job.skip, job.skip + 2 do lines[#lines + 1] = ui.Line { ui.Span("L" .. i):fg("red"), " " .. job.file.name } end
    ya.preview_widget(job, { ui.Text(lines):area(job.area), ui.Text { "R" }:area(job.area):align(ui.Align.RIGHT) })
  end,
  spot = function(self, job)
    ya.spot_table(job, ui.Table({ ui.Row({ "Custom" }):style(ui.Style():fg("green")), ui.Row { "  Name:", job.file.name } }))
  end,
}
"#,
        )
        .unwrap();
        let fe = cfg.join("plugins/marks.yazi");
        std::fs::create_dir_all(&fe).unwrap();
        std::fs::write(
            fe.join("main.lua"),
            r#"
local add = ya.sync(function(st, names) st.seen = st.seen or {}; for _, n in ipairs(names) do st.seen[n] = true end end)
local function setup(st)
  st.seen = {}
  Linemode:children_add(function(self)
    if st.seen[self._file.name] then return ui.Line { " ", ui.Span("M"):style(th.mgr.cwd or ui.Style():fg("yellow")) } end
    return ""
  end, 1500)
  Status:children_add(function() return ui.Span("[git]"):bold() end, 2500, Status.RIGHT)
end
function Linemode:size_name() return self._file.name .. "!" end
return {
  setup = setup,
  fetch = function(_, job)
    local names = {}
    for i, f in ipairs(job.files) do names[i] = f.name end
    add(names)
    return true
  end,
}
"#,
        )
        .unwrap();
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var("WISP_CONFIG_HOME", &cfg);
        }
        let th = crate::theme::Theme::from_table(&crate::config::PRESET_THEME.parse().unwrap());
        let rt = Runtime::new(
            &Config::from_table(&crate::config::PRESET_YAZI.parse().unwrap()),
            &th,
        )
        .unwrap();
        rt.run_init_src(r#"require("marks"):setup()"#).unwrap();

        let work = root.path().join("w");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("a.md"), "x").unwrap();
        let f = File::from_path(&work.join("a.md")).unwrap();

        // Previewer: widgets → lines (left text + right-aligned column).
        let out = rt.peek("glowish", &f, "text/markdown", 1, 10, 5, "");
        assert!(out.error.is_none(), "{:?}", out.error);
        let lines = out.lines.unwrap();
        assert_eq!(lines[0].to_string(), "L2 a.md  R");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));

        // Spotter.
        let rows = rt.spot("glowish", &f, "text/markdown").unwrap().unwrap();
        assert_eq!(rows[0], ("Custom".to_string(), String::new()));
        assert_eq!(rows[1], ("  Name:".to_string(), "a.md".to_string()));

        // Fetcher feeding a Linemode child; Status child; custom linemode.
        assert!(rt.component_active("Linemode"));
        let ctx = FileCtx {
            file: &f,
            hovered: false,
            in_current: true,
            in_preview: false,
            in_parent: false,
        };
        assert!(rt
            .component_children("Linemode", Some(&ctx), None, None)
            .is_empty());
        assert!(rt.fetch("marks", std::slice::from_ref(&f)).is_none());
        let kids = rt.component_children("Linemode", Some(&ctx), None, None);
        assert_eq!(kids[0].0, 1500);
        assert_eq!(
            kids[0].1.iter().map(|x| x.0.as_str()).collect::<String>(),
            " M"
        );
        let st = rt.component_children(
            "Status",
            None,
            Some(ratatui::layout::Rect::new(0, 0, 80, 1)),
            Some(1),
        );
        assert_eq!(st[0].1[0].0, "[git]");
        assert!(st[0].1[0].1.add_modifier.contains(Modifier::BOLD));
        assert!(rt.has_method("Linemode", "size_name"));
        let custom = rt
            .component_override("Linemode", "size_name", Some(&ctx), None)
            .unwrap();
        assert_eq!(custom[0].0, "a.md!");
    }

    #[test]
    fn runs_a_plugin_with_the_yazi_api() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("cfg");
        let pdir = cfg.join("plugins/hello.yazi");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("main.lua"),
            r#"
local get_hovered = ya.sync(function(state)
  state.calls = (state.calls or 0) + 1
  return cx.active.current.hovered.name, #cx.active.current.files
end)
return {
  entry = function(self, job)
    local name, n = get_hovered()
    local out = Command("printf"):arg({ "%s", "hi" }):output()
    local value, event = ya.input { title = "Name:", value = name }
    if event ~= 1 then return end
    local u = Url("/a/b/c.txt")
    ya.notify { title = "hello", content = out.stdout .. " " .. value .. " " .. u.name .. " " .. tostring(u.parent) .. " " .. n .. " " .. (job.args[1] or "") .. " " .. tostring(job.args.flag), timeout = 2 }
    ya.emit("cd", { "/tmp" })
  end,
}
"#,
        )
        .unwrap();
        let work = root.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("only.txt"), "x").unwrap();
        // SAFETY: only this test in this module touches these variables.
        unsafe {
            std::env::set_var("WISP_CONFIG_HOME", &cfg);
        }
        let mut rt = Runtime::new(
            &Config::from_table(&crate::config::PRESET_YAZI.parse().unwrap()),
            &crate::theme::Theme::from_table(&crate::config::PRESET_THEME.parse().unwrap()),
        )
        .unwrap();
        let cmd = Cmd::parse("plugin hello first --flag").unwrap();
        let s = snap(&work);
        match rt.start("hello", &cmd, &s) {
            Step::Input { title, value, .. } => {
                assert_eq!(title, "Name:");
                assert_eq!(value, "only.txt");
            }
            Step::Error(e) => panic!("{e}"),
            _ => panic!("expected input"),
        }
        assert!(rt.is_running());
        match rt.resume(Resume::Input(Some("typed".into())), &s) {
            Step::Done => {}
            Step::Error(e) => panic!("{e}"),
            _ => panic!("expected done"),
        }
        let (emits, notes) = rt.drain();
        assert_eq!(emits[0].name, "cd");
        assert_eq!(emits[0].args, vec!["/tmp"]);
        assert_eq!(notes[0].2, "hi typed c.txt /a/b 1 first true");
    }
}

#[cfg(test)]
mod table_probe {
    use super::*;

    #[test]
    fn table_cells_with_urls() {
        let rt = Runtime::new(
            &Config::from_table(&crate::config::PRESET_YAZI.parse().unwrap()),
            &crate::theme::Theme::from_table(&crate::config::PRESET_THEME.parse().unwrap()),
        )
        .unwrap();
        let v: Value = rt
            .lua
            .load(r#"return { ui.Table({ ui.Row { "  1", "", Url("/boot"), "vfat" } }):area(ui.Rect { x = 0, y = 0, w = 100, h = 5 }):header(ui.Row({ "Src", "Label", "Dist", "FSType" })):widths { ui.Constraint.Length(20), ui.Constraint.Length(20), ui.Constraint.Percentage(50), ui.Constraint.Length(20) } }"#)
            .eval()
            .unwrap();
        let d = widget_draws(&v);
        let Draw::Lines { rows, .. } = &d[0] else {
            panic!()
        };
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.0.iter().map(|x| x.0.as_str()).collect())
            .collect();
        assert!(text[1].contains("/boot"), "{text:?}");
        assert!(text[1].contains("vfat"), "{text:?}");
    }
}

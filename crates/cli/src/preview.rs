//! Off-thread previews.
//!
//! The UI sends a [`Request`] (file + scroll offset + pane size) and goes on;
//! a worker builds the [`PreviewContent`] and posts it back keyed by [`Key`],
//! where it lands in an LRU so revisiting a file is instant. Requests are
//! coalesced: if the cursor moved on while the worker was busy, only the
//! newest request is served. A second, low-priority worker prefetches the
//! neighbours of the hovered file so the next `j` already has its preview.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use ratatui::layout::Size;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::Resize;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

use crate::event::Msg;
use crate::util::{on_path, readable_size};

/// Identity of a preview: same file version, scroll offset and size.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key {
    pub url: PathBuf,
    pub mtime: i64,
    pub len: u64,
    pub skip: usize,
    pub w: u16,
    pub h: u16,
}

pub enum PreviewContent {
    /// Lines of (possibly highlighted) text. `skip` is the offset actually
    /// used, which may be lower than requested near the end of the file.
    Text {
        lines: Vec<Line<'static>>,
        skip: usize,
    },
    Image(Box<Protocol>),
    Error(String),
    Empty,
}

#[derive(Clone)]
pub struct Request {
    pub key: Key,
    pub mime: String,
    /// The configured previewer (`code`, `image`, `piper -- …`), or empty
    /// to decide from the MIME type.
    pub previewer: String,
    pub picker: Option<Picker>,
    pub tab_size: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub quality: u8,
}

struct Shared {
    syntaxes: SyntaxSet,
    theme: Theme,
}

/// The `.tmTheme` to highlight with, set once at startup.
static THEME_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Syntax definitions and theme, loaded on first use (tens of milliseconds of
/// CPU that must not compete with the first frame).
fn shared() -> &'static Shared {
    static S: OnceLock<Shared> = OnceLock::new();
    S.get_or_init(|| {
        let theme_path = THEME_PATH.get().and_then(|p| p.as_deref());
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let theme = theme_path
            .and_then(|p| ThemeSet::get_theme(p).ok())
            .unwrap_or_else(|| {
                let mut ts = ThemeSet::load_defaults();
                ts.themes
                    .remove("base16-ocean.dark")
                    .or_else(|| ts.themes.into_values().next())
                    .expect("syntect ships themes")
            });
        Shared { syntaxes, theme }
    })
}

/// A coalescing single-slot queue.
struct Slot {
    req: Mutex<Option<Request>>,
    cv: Condvar,
}

impl Slot {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            req: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn put(&self, r: Request) {
        *self.req.lock().unwrap() = Some(r);
        self.cv.notify_one();
    }

    fn take(&self) -> Request {
        let mut g = self.req.lock().unwrap();
        loop {
            if let Some(r) = g.take() {
                return r;
            }
            g = self.cv.wait(g).unwrap();
        }
    }
}

pub struct Previewer {
    main: Arc<Slot>,
    pre: Arc<Mutex<VecDeque<Request>>>,
    pre_cv: Arc<Condvar>,
    cache: HashMap<Key, Arc<PreviewContent>>,
    order: VecDeque<Key>,
    inflight: Option<Key>,
    /// Held until the workers are started.
    tx: Option<Sender<Msg>>,
}

const CACHE_CAP: usize = 48;

impl Previewer {
    /// Create the previewer; its worker threads start on first use.
    pub fn spawn(tx: Sender<Msg>, theme: Option<PathBuf>) -> Self {
        let _ = THEME_PATH.set(theme);
        Self {
            main: Slot::new(),
            pre: Arc::new(Mutex::new(VecDeque::new())),
            pre_cv: Arc::new(Condvar::new()),
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: None,
            tx: Some(tx),
        }
    }

    fn start_workers(&mut self) {
        let Some(tx) = self.tx.take() else { return };
        {
            let (slot, tx) = (self.main.clone(), tx.clone());
            std::thread::spawn(move || loop {
                let r = slot.take();
                let key = r.key.clone();
                let content = build(&r);
                if tx.send(Msg::Preview { key, content }).is_err() {
                    return;
                }
            });
        }
        let (q, cv) = (self.pre.clone(), self.pre_cv.clone());
        std::thread::spawn(move || loop {
            let r = {
                let mut g = q.lock().unwrap();
                loop {
                    if let Some(r) = g.pop_front() {
                        break r;
                    }
                    g = cv.wait(g).unwrap();
                }
            };
            let key = r.key.clone();
            let content = build(&r);
            if tx.send(Msg::Preview { key, content }).is_err() {
                return;
            }
        });
    }

    pub fn get(&self, key: &Key) -> Option<Arc<PreviewContent>> {
        self.cache.get(key).cloned()
    }

    /// Request a preview unless it is cached or already being built.
    pub fn request(&mut self, r: Request) {
        if self.cache.contains_key(&r.key) || self.inflight.as_ref() == Some(&r.key) {
            return;
        }
        self.start_workers();
        self.inflight = Some(r.key.clone());
        self.main.put(r);
    }

    /// Queue low-priority prefetches (replacing older ones).
    pub fn prefetch(&mut self, reqs: Vec<Request>) {
        if reqs.is_empty() {
            return;
        }
        self.start_workers();
        let mut q = self.pre.lock().unwrap();
        q.clear();
        for r in reqs {
            if !self.cache.contains_key(&r.key) {
                q.push_back(r);
            }
        }
        drop(q);
        self.pre_cv.notify_one();
    }

    pub fn insert(&mut self, key: Key, content: PreviewContent) -> Arc<PreviewContent> {
        if self.inflight.as_ref() == Some(&key) {
            self.inflight = None;
        }
        let c = Arc::new(content);
        if self.cache.insert(key.clone(), c.clone()).is_none() {
            self.order.push_back(key);
            while self.order.len() > CACHE_CAP {
                if let Some(old) = self.order.pop_front() {
                    self.cache.remove(&old);
                }
            }
        }
        c
    }

    pub fn invalidate(&mut self, url: &Path) {
        self.cache.retain(|k, _| k.url != url);
        self.order.retain(|k| k.url != url);
    }
}

// --------------------------------------------------------------------------
// Builders
// --------------------------------------------------------------------------

fn is_text_mime(m: &str) -> bool {
    m.starts_with("text/")
        || matches!(
            m,
            "application/json"
                | "application/ndjson"
                | "application/javascript"
                | "application/mbox"
                | "application/wine-extension-ini"
                | "application/toml"
                | "application/xml"
                | "application/sh"
                | "application/shellscript"
        )
}

pub fn is_archive(m: &str) -> bool {
    let Some(sub) = m.strip_prefix("application/") else {
        return false;
    };
    matches!(
        sub,
        "zip"
            | "rar"
            | "tar"
            | "gzip"
            | "xz"
            | "zstd"
            | "lzma"
            | "compress"
            | "archive"
            | "cpio"
            | "arj"
            | "xar"
            | "java-archive"
            | "debian-package"
            | "rpm"
            | "redhat-package-manager"
            | "android.package-archive"
            | "iso9660-image"
    ) || sub.starts_with("7z")
        || sub.starts_with("bzip")
        || sub.starts_with("ms-cab")
        || sub.starts_with("vnd.rar")
}

fn empty_file() -> PreviewContent {
    PreviewContent::Text {
        lines: vec![Line::styled(
            "Empty file",
            Style::default().add_modifier(Modifier::REVERSED),
        )],
        skip: 0,
    }
}

/// Dispatch on a configured previewer name, like yazi's `run = "…"`.
fn by_name(r: &Request) -> Option<Result<PreviewContent, String>> {
    let run = r.previewer.trim();
    let name = run.split_whitespace().next().unwrap_or("");
    Some(match name {
        "folder" | "noop" => Ok(PreviewContent::Empty),
        "empty" => Ok(empty_file()),
        "code" | "json" => text(r),
        "image" => image_from(r, &r.key.url),
        "magick" | "svg" => convert_then_image(r, "magick"),
        "font" => convert_then_image(r, "font"),
        "video" => convert_then_image(r, "video"),
        "pdf" => convert_then_image(r, "pdf"),
        "archive" => archive(r),
        "file" | "null" | "vfs" => file_info(r),
        "@image" => image_from(r, Path::new(run.split_once(' ').map(|x| x.1).unwrap_or(""))),
        "piper" if crate::plugin::find_plugin("piper").is_none() => {
            piper(r, run.split_once(" -- ").map(|x| x.1).unwrap_or(""))
        }
        other if crate::plugin::find_plugin(other).is_some() => {
            let args = run
                .split_once(char::is_whitespace)
                .map(|x| x.1)
                .unwrap_or("");
            lua_peek(r, other, args)
        }
        _ => return None,
    })
}

/// Config and theme for the preview thread's own Lua state.
static LUA_ENV: OnceLock<(crate::config::Config, crate::theme::Theme)> = OnceLock::new();

pub fn set_lua_env(cfg: crate::config::Config, th: crate::theme::Theme) {
    let _ = LUA_ENV.set((cfg, th));
}

thread_local! {
    static LUA: std::cell::RefCell<Option<crate::plugin::Runtime>> = const { std::cell::RefCell::new(None) };
}

/// Run a Lua previewer plugin (`peek`) on this worker thread.
fn lua_peek(r: &Request, name: &str, args: &str) -> Result<PreviewContent, String> {
    let file = crate::fs::File::from_path(&r.key.url).map_err(|e| e.to_string())?;
    let mut skip = r.key.skip;
    for _ in 0..2 {
        let out = LUA.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                let (cfg, th) = LUA_ENV.get().ok_or("plugins unavailable")?;
                let rt = crate::plugin::Runtime::new(cfg, th).map_err(|e| e.to_string())?;
                rt.mark_preview_thread();
                *slot = Some(rt);
            }
            let rt = slot.as_ref().expect("just created");
            Ok::<_, String>(rt.peek(name, &file, &r.mime, skip, r.key.w, r.key.h, args))
        })?;
        if let Some(e) = out.error {
            return Err(format!("{name}: {e}"));
        }
        // `ya.emit("peek", { n })`: re-run once with the corrected offset.
        if let Some(n) = out.peek {
            if n != skip {
                skip = n;
                continue;
            }
        }
        if let Some(native) = out.native {
            let mut nr = r.clone();
            nr.previewer = native.clone();
            nr.key.skip = skip;
            if native == "spot" || crate::plugin::find_plugin(&native).is_some() {
                return file_info(&nr);
            }
            return by_name(&nr).unwrap_or_else(|| file_info(&nr));
        }
        if let Some(img) = out.image {
            return image_from(r, &img);
        }
        return Ok(PreviewContent::Text {
            lines: out.lines.unwrap_or_default(),
            skip,
        });
    }
    Err(format!("{name}: peek kept changing its offset"))
}

fn build(r: &Request) -> PreviewContent {
    if !r.previewer.is_empty() {
        if let Some(res) = by_name(r) {
            return res.unwrap_or_else(PreviewContent::Error);
        }
    }
    let m = r.mime.as_str();
    let path = &r.key.url;
    let res = if m == "inode/directory" {
        Ok(PreviewContent::Empty)
    } else if m == "inode/empty" {
        Ok(empty_file())
    } else if is_text_mime(m) {
        text(r)
    } else if m.starts_with("image/") {
        match m {
            "image/svg+xml" | "image/avif" | "image/heic" | "image/heif" | "image/jxl" => {
                convert_then_image(r, "magick")
            }
            _ => image_from(r, path),
        }
    } else if m.starts_with("video/") {
        convert_then_image(r, "video")
    } else if m == "application/pdf" {
        convert_then_image(r, "pdf")
    } else if is_archive(m)
        || path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
    {
        archive(r)
    } else if m.starts_with("font/") || m == "application/ms-opentype" {
        convert_then_image(r, "font")
    } else {
        file_info(r)
    };
    res.unwrap_or_else(PreviewContent::Error)
}

fn syn_style(s: syntect::highlighting::Style) -> Style {
    let fg = s.foreground;
    let mut st = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if s.font_style.contains(FontStyle::BOLD) {
        st = st.add_modifier(Modifier::BOLD);
    }
    if s.font_style.contains(FontStyle::ITALIC) {
        st = st.add_modifier(Modifier::ITALIC);
    }
    if s.font_style.contains(FontStyle::UNDERLINE) {
        st = st.add_modifier(Modifier::UNDERLINED);
    }
    st
}

fn expand_tabs(s: &str, tab: usize) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    s.replace('\t', &" ".repeat(tab))
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect()
}

/// Read up to `n` lines of a text file, each capped in length.
fn read_lines(path: &Path, n: usize) -> Result<Vec<String>, String> {
    let f = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(f);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    let mut scanned = 0usize;
    while out.len() < n {
        buf.clear();
        let got = r
            .by_ref()
            .take(64 * 1024)
            .read_until(b'\n', &mut buf)
            .map_err(|e| e.to_string())?;
        if got == 0 {
            break;
        }
        scanned += got;
        if buf.contains(&0) && out.is_empty() && scanned < 8192 {
            return Err("binary".into());
        }
        // A line longer than our cap: drain the rest of it.
        if !buf.ends_with(b"\n") && got == 64 * 1024 {
            let mut sink = Vec::new();
            let _ = r.read_until(b'\n', &mut sink);
        }
        let mut s = String::from_utf8_lossy(&buf).into_owned();
        while s.ends_with('\n') || s.ends_with('\r') {
            s.pop();
        }
        s.truncate(s.char_indices().nth(2000).map_or(s.len(), |(i, _)| i));
        out.push(s);
        if scanned > 64 << 20 {
            break;
        }
    }
    Ok(out)
}

fn text(r: &Request) -> Result<PreviewContent, String> {
    let h = r.key.h as usize;
    let want = r.key.skip + h;
    let lines = match read_lines(&r.key.url, want) {
        Ok(l) => l,
        Err(e) if e == "binary" => return file_info(r),
        Err(e) => return Err(e),
    };
    // Scrolled past the end: pin the last page to the bottom (yazi's
    // upper-bound peek).
    let skip = if r.key.skip > 0 && lines.len() < want {
        lines.len().saturating_sub(h)
    } else {
        r.key.skip
    };
    let sh = shared();
    let syntax = sh
        .syntaxes
        .find_syntax_for_file(&r.key.url)
        .ok()
        .flatten()
        .or_else(|| {
            lines
                .first()
                .and_then(|l| sh.syntaxes.find_syntax_by_first_line(l))
        });
    let big = lines.iter().map(String::len).sum::<usize>() > 2 << 20;
    let out: Vec<Line<'static>> = match syntax {
        Some(syn) if !big && syn.name != "Plain Text" => {
            let mut hl = HighlightLines::new(syn, &sh.theme);
            let mut out = Vec::with_capacity(h);
            for (i, l) in lines.iter().enumerate() {
                let with_nl = format!("{l}\n");
                let regions = hl
                    .highlight_line(&with_nl, &sh.syntaxes)
                    .unwrap_or_default();
                if i < skip {
                    continue;
                }
                let spans: Vec<Span<'static>> = regions
                    .into_iter()
                    .map(|(st, t)| {
                        Span::styled(
                            clean(&expand_tabs(t.trim_end_matches('\n'), r.tab_size)),
                            syn_style(st),
                        )
                    })
                    .collect();
                out.push(Line::from(spans));
            }
            out
        }
        _ => lines
            .iter()
            .skip(skip)
            .map(|l| Line::raw(clean(&expand_tabs(l, r.tab_size))))
            .collect(),
    };
    Ok(PreviewContent::Text { lines: out, skip })
}

fn image_from(r: &Request, path: &Path) -> Result<PreviewContent, String> {
    let Some(picker) = &r.picker else {
        return file_info(r);
    };
    let img = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    // Downscale first: encoding a 6000px photo for the terminal is wasted work.
    let fs = picker.font_size();
    let max_w = (r.key.w as u32 * fs.width as u32).min(r.max_width).max(1);
    let max_h = (r.key.h as u32 * fs.height as u32).min(r.max_height).max(1);
    let img = if img.width() > max_w || img.height() > max_h {
        img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let proto = picker
        .new_protocol(img, Size::new(r.key.w, r.key.h), Resize::Fit(None))
        .map_err(|e| e.to_string())?;
    Ok(PreviewContent::Image(Box::new(proto)))
}

fn thumb_path(r: &Request, kind: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    r.key.url.hash(&mut h);
    r.key.mtime.hash(&mut h);
    r.key.len.hash(&mut h);
    kind.hash(&mut h);
    if kind == "pdf" {
        r.key.skip.hash(&mut h);
    }
    let dir = crate::config::cache_dir();
    let _ = fs::create_dir_all(&dir);
    dir.join(format!("{:016x}.jpg", h.finish()))
}

fn run_quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Render a thumbnail with an external tool (cached on disk), then show it.
fn convert_then_image(r: &Request, kind: &str) -> Result<PreviewContent, String> {
    if r.picker.is_none() {
        return file_info(r);
    }
    let out = thumb_path(r, kind);
    if !out.is_file() {
        let src = &r.key.url;
        let size = r.max_width.max(r.max_height).to_string();
        let ok = match kind {
            "video" if on_path("ffmpegthumbnailer") => run_quiet(
                Command::new("ffmpegthumbnailer")
                    .arg("-i")
                    .arg(src)
                    .arg("-o")
                    .arg(&out)
                    .args(["-s", &size, "-t", "10%", "-q", "6", "-c", "jpeg"]),
            ),
            "video" if on_path("ffmpeg") => run_quiet(
                Command::new("ffmpeg")
                    .args(["-v", "quiet", "-ss", "5", "-i"])
                    .arg(src)
                    .args(["-frames:v", "1", "-vf"])
                    .arg(format!("scale={size}:-2"))
                    .arg("-y")
                    .arg(&out),
            ),
            "pdf" if on_path("pdftoppm") => {
                let page = (r.key.skip + 1).to_string();
                let prefix = out.with_extension("");
                run_quiet(
                    Command::new("pdftoppm")
                        .args(["-singlefile", "-jpeg", "-jpegopt"])
                        .arg(format!("quality={}", r.quality))
                        .args(["-f", &page, "-l", &page, "-scale-to", &size])
                        .arg(src)
                        .arg(&prefix),
                )
            }
            "magick" | "font" if on_path("magick") => {
                let mut c = Command::new("magick");
                if kind == "font" {
                    c.args(["-size", "800x560", "-gravity", "center", "-pointsize", "40"])
                        .arg("-font")
                        .arg(src)
                        .arg("label:ABCDEFGHIJKLM\nNOPQRSTUVWXYZ\nabcdefghijklm\nnopqrstuvwxyz\n1234567890 !?&");
                } else {
                    c.arg(src)
                        .args(["-auto-orient", "-resize"])
                        .arg(format!("{size}x{size}>"));
                }
                c.args(["-quality", &r.quality.to_string()]).arg(&out);
                run_quiet(&mut c)
            }
            _ => false,
        };
        if !ok || !out.is_file() {
            if kind == "pdf" && r.key.skip > 0 {
                return Err("No more pages".into());
            }
            return file_info(r);
        }
    }
    image_from(r, &out)
}

fn archive(r: &Request) -> Result<PreviewContent, String> {
    let path = &r.key.url;
    let mut entries: Vec<(String, u64, bool)> = Vec::new();
    let is_zip = path
        .extension()
        .map(|e| {
            let e = e.to_string_lossy().to_ascii_lowercase();
            matches!(
                e.as_str(),
                "zip" | "jar" | "apk" | "war" | "xpi" | "cbz" | "epub"
            )
        })
        .unwrap_or(false);
    let mut done = false;
    if is_zip {
        if let Ok(f) = fs::File::open(path) {
            if let Ok(mut z) = zip::ZipArchive::new(f) {
                for i in 0..z.len() {
                    if let Ok(e) = z.by_index_raw(i) {
                        entries.push((e.name().to_string(), e.size(), e.is_dir()));
                    }
                }
                done = true;
            }
        }
    }
    if !done {
        let tool = ["bsdtar", "7z", "tar"].into_iter().find(|t| on_path(t));
        let Some(tool) = tool else {
            return file_info(r);
        };
        let out = if tool == "7z" {
            Command::new("7z")
                .args(["l", "-ba", "-slt"])
                .arg(path)
                .output()
        } else {
            Command::new(tool).arg("-tvf").arg(path).output()
        }
        .map_err(|e| e.to_string())?;
        let s = String::from_utf8_lossy(&out.stdout);
        if tool == "7z" {
            let (mut name, mut size, mut dir) = (String::new(), 0u64, false);
            for l in s.lines() {
                if let Some(v) = l.strip_prefix("Path = ") {
                    name = v.to_string();
                } else if let Some(v) = l.strip_prefix("Size = ") {
                    size = v.parse().unwrap_or(0);
                } else if let Some(v) = l.strip_prefix("Folder = ") {
                    dir = v == "+";
                } else if l.is_empty() && !name.is_empty() {
                    entries.push((std::mem::take(&mut name), size, dir));
                }
            }
            if !name.is_empty() {
                entries.push((name, size, dir));
            }
        } else {
            // `-rw-r--r--  0 user group  1234 Jan  1 00:00 name`
            for l in s.lines() {
                let parts: Vec<&str> = l.split_whitespace().collect();
                if parts.len() >= 9 {
                    let size = parts[4].parse().unwrap_or(0);
                    let name = parts[8..].join(" ");
                    let dir = l.starts_with('d');
                    entries.push((name, size, dir));
                }
            }
        }
        if entries.is_empty() && !out.status.success() {
            return file_info(r);
        }
    }
    let h = r.key.h as usize;
    let skip = r.key.skip.min(entries.len().saturating_sub(h));
    let lines = entries
        .iter()
        .skip(skip)
        .take(h)
        .map(|(n, s, d)| {
            let size = if *d { String::new() } else { readable_size(*s) };
            Line::from(vec![
                Span::styled(
                    clean(n),
                    if *d {
                        Style::default().fg(Color::Blue)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("  {size}"),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ])
        })
        .collect();
    Ok(PreviewContent::Text { lines, skip })
}

/// The `piper` previewer: run a shell command (`$1` = the file, `$w`/`$h`
/// = the pane size) and show its output, ANSI colours included.
fn piper(r: &Request, cmd: &str) -> Result<PreviewContent, String> {
    if cmd.trim().is_empty() {
        return Err("piper: no command given (use `piper -- <command>`)".into());
    }
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .arg("piper")
        .arg(&r.key.url)
        .env("w", r.key.w.to_string())
        .env("h", r.key.h.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("piper: {e}"))?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let all = ansi_lines(&expand_tabs(&text, r.tab_size));
    let h = r.key.h as usize;
    let skip = if r.key.skip + h > all.len() {
        all.len().saturating_sub(h).min(r.key.skip)
    } else {
        r.key.skip
    };
    let lines = all.into_iter().skip(skip).take(h).collect();
    Ok(PreviewContent::Text { lines, skip })
}

/// Parse text with ANSI SGR escapes into styled lines.
pub fn ansi_lines(s: &str) -> Vec<Line<'static>> {
    fn color(it: &mut std::slice::Iter<'_, u16>, base: u16) -> Option<Color> {
        match it.next()? {
            5 => it.next().map(|&n| Color::Indexed(n as u8)),
            2 => {
                let r = *it.next()? as u8;
                let g = *it.next()? as u8;
                let b = *it.next()? as u8;
                Some(Color::Rgb(r, g, b))
            }
            _ => {
                let _ = base;
                None
            }
        }
    }
    const NAMED: [Color; 8] = [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
    ];
    const BRIGHT: [Color; 8] = [
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ];
    let mut lines = Vec::new();
    let mut style = Style::default();
    for raw in s.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut cur = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                if !c.is_control() {
                    cur.push(c);
                }
                continue;
            }
            if chars.peek() != Some(&'[') {
                // Not CSI (e.g. OSC): skip to its terminator.
                for n in chars.by_ref() {
                    if n == '\x07' || n == '\\' {
                        break;
                    }
                }
                continue;
            }
            chars.next();
            let mut params = String::new();
            let mut fin = ' ';
            for n in chars.by_ref() {
                if n.is_ascii_digit() || n == ';' || n == ':' {
                    params.push(n);
                } else {
                    fin = n;
                    break;
                }
            }
            if fin != 'm' {
                continue;
            }
            if !cur.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut cur), style));
            }
            let nums: Vec<u16> = if params.is_empty() {
                vec![0]
            } else {
                params
                    .split([';', ':'])
                    .map(|p| p.parse().unwrap_or(0))
                    .collect()
            };
            let mut it = nums.iter();
            while let Some(&n) = it.next() {
                style = match n {
                    0 => Style::default(),
                    1 => style.add_modifier(Modifier::BOLD),
                    2 => style.add_modifier(Modifier::DIM),
                    3 => style.add_modifier(Modifier::ITALIC),
                    4 => style.add_modifier(Modifier::UNDERLINED),
                    7 => style.add_modifier(Modifier::REVERSED),
                    9 => style.add_modifier(Modifier::CROSSED_OUT),
                    22 => style.remove_modifier(Modifier::BOLD | Modifier::DIM),
                    23 => style.remove_modifier(Modifier::ITALIC),
                    24 => style.remove_modifier(Modifier::UNDERLINED),
                    27 => style.remove_modifier(Modifier::REVERSED),
                    30..=37 => style.fg(NAMED[(n - 30) as usize]),
                    38 => match color(&mut it, 38) {
                        Some(c) => style.fg(c),
                        None => style,
                    },
                    39 => style.fg(Color::Reset),
                    40..=47 => style.bg(NAMED[(n - 40) as usize]),
                    48 => match color(&mut it, 48) {
                        Some(c) => style.bg(c),
                        None => style,
                    },
                    49 => style.bg(Color::Reset),
                    90..=97 => style.fg(BRIGHT[(n - 90) as usize]),
                    100..=107 => style.bg(BRIGHT[(n - 100) as usize]),
                    _ => style,
                };
            }
        }
        if !cur.is_empty() {
            spans.push(Span::styled(cur, style));
        }
        lines.push(Line::from(spans));
    }
    if lines.last().is_some_and(|l| l.spans.is_empty()) {
        lines.pop();
    }
    lines
}

/// Greedy word wrap (ratatui's `Wrap { trim: true }` behaviour): break at
/// spaces, hard-split words longer than the line.
pub fn word_wrap(s: &str, width: usize) -> Vec<String> {
    use crate::util::width as w;
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ').filter(|x| !x.is_empty()) {
        let mut word = word.to_string();
        let sep = usize::from(!cur.is_empty());
        if w(&cur) + sep + w(&word) <= width {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(&word);
            continue;
        }
        if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        while w(&word) > width {
            let part = crate::util::truncate(&word, width).to_string();
            if part.is_empty() {
                break;
            }
            word = word[part.len()..].to_string();
            out.push(part);
        }
        cur = word;
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

/// Fallback, like yazi's `file` previewer: what `file(1)` says, wrapped.
fn file_info(r: &Request) -> Result<PreviewContent, String> {
    let mut text = String::new();
    if on_path("file") {
        if let Ok(out) = Command::new("file")
            .arg("-bL")
            .arg("--")
            .arg(&r.key.url)
            .stderr(Stdio::null())
            .output()
        {
            text = String::from_utf8_lossy(&out.stdout).into_owned();
        }
    }
    if text.trim().is_empty() {
        text = r.mime.clone();
    }
    let width = (r.key.w as usize).max(1);
    let mut lines = vec![
        Line::raw("----- File Type Classification -----"),
        Line::raw(""),
    ];
    for l in text.lines() {
        for w in word_wrap(&clean(l), width) {
            lines.push(Line::raw(w));
        }
    }
    Ok(PreviewContent::Text { lines, skip: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(path: &Path, mime: &str, skip: usize) -> Request {
        Request {
            key: Key {
                url: path.to_path_buf(),
                mtime: 0,
                len: 0,
                skip,
                w: 40,
                h: 5,
            },
            mime: mime.into(),
            previewer: String::new(),
            picker: None,
            tab_size: 2,
            max_width: 600,
            max_height: 900,
            quality: 75,
        }
    }

    #[test]
    fn text_preview_and_upper_bound() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.rs");
        let body: String = (0..20).map(|i| format!("fn f{i}() {{}}\n")).collect();
        fs::write(&p, body).unwrap();
        match build(&req(&p, "text/rust", 0)) {
            PreviewContent::Text { lines, skip } => {
                assert_eq!(lines.len(), 5);
                assert_eq!(skip, 0);
            }
            _ => panic!(),
        }
        match build(&req(&p, "text/rust", 100)) {
            PreviewContent::Text { lines, skip } => {
                assert_eq!(skip, 15);
                assert_eq!(lines.len(), 5);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn ansi_and_piper() {
        let l = ansi_lines("\x1b[1;31mred\x1b[0m plain\n\x1b[38;2;1;2;3mrgb\x1b[m");
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].spans[0].content, "red");
        assert_eq!(l[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(l[1].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.txt");
        fs::write(&p, "one\ntwo\n").unwrap();
        let mut r = req(&p, "text/plain", 0);
        r.previewer = "piper -- printf 'W=%s %s' \"$w\" \"$(cat \"$1\" | wc -l)\"".into();
        match build(&r) {
            PreviewContent::Text { lines, .. } => assert_eq!(lines[0].to_string(), "W=40 2"),
            _ => panic!(),
        }
    }

    #[test]
    fn zip_listing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.zip");
        let f = fs::File::create(&p).unwrap();
        let mut z = zip::ZipWriter::new(f);
        z.start_file::<_, ()>("hello.txt", Default::default())
            .unwrap();
        std::io::Write::write_all(&mut z, b"hi").unwrap();
        z.finish().unwrap();
        match build(&req(&p, "application/zip", 0)) {
            PreviewContent::Text { lines, .. } => {
                assert!(lines[0].to_string().contains("hello.txt"))
            }
            _ => panic!(),
        }
    }
}

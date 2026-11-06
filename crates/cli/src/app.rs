//! Application state and the main loop.
//!
//! The loop blocks on a single channel (see [`crate::event`]), handles every
//! message that is ready, re-syncs derived state (which folders to load, which
//! preview to build, what to watch), and repaints only when something changed.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableMouseCapture, Event, KeyEventKind, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use notify::Watcher;
use ratatui::layout::Rect;

use ratatui_image::picker::{Picker, ProtocolType};

use crate::config::{Config, Opener};
use crate::event::{InputThread, Msg};
use crate::folder::{Folder, Stage};
use crate::fs::{read_dir, sort_files, File};
use crate::input::{Input, Mode as InputMode};
use crate::keymap::{Binding, Cmd, Key, Keymap, Layer, Lookup};
use crate::preview::{self, PreviewContent, Previewer};
use crate::tab::{Pref, Tab};
use crate::tasks::{State as TaskState, TaskEvent, TaskInfo};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

pub struct Notification {
    pub level: Level,
    pub title: String,
    pub body: String,
    pub until: Instant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InputKind {
    Cd,
    Create {
        dir: bool,
        force: bool,
    },
    Rename {
        from: PathBuf,
        force: bool,
    },
    Filter {
        smart: bool,
        insensitive: bool,
        prev: Option<String>,
    },
    Find {
        prev: bool,
        smart: bool,
        insensitive: bool,
    },
    Search {
        via: String,
    },
    Shell {
        block: bool,
        orphan: bool,
    },
    TabRename,
    HelpFilter,
    /// `ya.input` from a Lua plugin.
    Plugin,
    /// `ya.input { realtime = true }`: every edit resumes the plugin.
    PluginRealtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    TopCenter,
    Hovered,
}

pub struct InputState {
    pub kind: InputKind,
    pub input: Input,
    pub title: String,
    pub origin: Origin,
    /// Path completions (for `cd --interactive`).
    pub cmp: Vec<String>,
    pub cmp_cursor: usize,
    pub cmp_shown: bool,
}

#[derive(Clone, Debug)]
pub enum ConfirmKind {
    Trash(Vec<PathBuf>),
    Delete(Vec<PathBuf>),
    Quit {
        no_cwd: bool,
    },
    CreateOverwrite {
        path: PathBuf,
        dir: bool,
    },
    RenameOverwrite {
        from: PathBuf,
        to: PathBuf,
    },
    /// `ya.confirm` from a Lua plugin.
    Plugin,
}

pub struct ConfirmState {
    pub kind: ConfirmKind,
    pub title: String,
    pub body: String,
    pub list: Vec<String>,
    pub offset: usize,
}

pub struct PickState {
    pub title: String,
    pub items: Vec<Opener>,
    pub cursor: usize,
    pub targets: Vec<PathBuf>,
}

pub struct WhichState {
    pub layer: Layer,
    pub keys: Vec<Key>,
    pub cands: Vec<Binding>,
    /// Shown for `ya.which` from a Lua plugin (answers resume it).
    pub plugin: bool,
    /// `ya.which { silent = true }`: wait for a key without a popup.
    pub silent: bool,
}

pub struct HelpState {
    pub layer: Layer,
    pub cursor: usize,
    pub offset: usize,
    pub filter: String,
}

pub struct TasksView {
    pub cursor: usize,
}

pub struct SpotState {
    pub url: PathBuf,
    pub rows: Vec<(String, String)>,
    pub cursor: usize,
}

/// Work that needs the terminal handed to another program.
pub enum Fg {
    /// Run `sh -c script` in the foreground.
    Shell {
        script: String,
        args: Vec<String>,
        cwd: PathBuf,
        wait_key: bool,
    },
    Fzf,
    Zoxide,
    BulkRename(Vec<PathBuf>),
    Suspend,
    /// A Lua plugin called `ya.hide()`.
    PluginHide,
}

#[derive(Default)]
pub struct Yanked {
    pub cut: bool,
    pub paths: BTreeSet<PathBuf>,
}

/// Screen regions, recomputed on every resize/tab change.
#[derive(Clone, Copy, Default)]
pub struct Areas {
    pub header: Rect,
    pub tabs: Option<Rect>,
    pub chunks: [Rect; 3],
    pub parent: Rect,
    pub current: Rect,
    pub preview: Rect,
    pub status: Rect,
}

pub struct App {
    pub cfg: Config,
    pub th: Theme,
    pub km: Keymap,
    pub tabs: Vec<Tab>,
    pub cur: usize,
    pub yanked: Yanked,

    pub tx: Sender<Msg>,
    rx: Receiver<Msg>,
    input_thread: Option<InputThread>,

    watcher: Option<notify::RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    pending_reload: HashMap<PathBuf, Instant>,
    loading: HashSet<PathBuf>,

    pub previewer: Previewer,
    pub preview_skip: usize,
    preview_for: Option<PathBuf>,
    pub preview_key: Option<preview::Key>,
    pub preview_content: Option<Arc<PreviewContent>>,
    pub picker: Option<Picker>,
    picker_tried: bool,
    want_picker: bool,

    pub tasks: Vec<TaskInfo>,
    next_task: u64,
    pub next_search: u64,

    pub mimes: HashMap<PathBuf, String>,
    mime_pending: HashSet<PathBuf>,
    pub dir_sizes: HashMap<PathBuf, (u64, u64)>,

    pub input: Option<InputState>,
    pub confirm: Option<ConfirmState>,
    pub pick: Option<PickState>,
    pub which: Option<WhichState>,
    pub help: Option<HelpState>,
    pub tasks_view: Option<TasksView>,
    pub spot: Option<SpotState>,
    pub notes: Vec<Notification>,
    pub fg: Option<Fg>,

    pub size: (u16, u16),
    pub areas: Areas,
    /// Preview pane hidden/maximized (`toggle-pane`).
    pub pane: [bool; 3],
    pub quit: bool,
    pub write_cwd: bool,
    pub cwd_file: Option<PathBuf>,
    pub chooser_file: Option<PathBuf>,
    pub chosen: Option<Vec<PathBuf>>,
    dirty: bool,
    last_title: String,
    /// File to hover once its directory finishes loading.
    pub pending_hover: HashMap<PathBuf, PathBuf>,
    after_fg: Option<AfterFg>,
    pending_refresh: HashSet<PathBuf>,
    painted: bool,
    after_plugin: Option<crate::plugin::Step>,
    /// mtime of the shared yank file we last wrote/read.
    pub yank_stamp: Option<std::time::SystemTime>,
    force_clear: bool,
    /// The Lua plugin runtime, created on first use.
    pub lua: Option<crate::plugin::Runtime>,
    /// Last cwd/hovered/tab reported to `ps` subscribers.
    ps_seen: (PathBuf, Option<PathBuf>, usize),
    plugin_cache: HashMap<String, bool>,
    fetched: HashSet<(String, PathBuf, i64)>,
    /// Border widgets added by a Lua Tab layout (full-border).
    pub lua_borders: Vec<(Rect, i64, ratatui::style::Style, bool)>,
    /// A plugin replaced `Status.redraw` with nothing (no-status).
    pub status_hidden: bool,
    /// An image a plugin asked to show in the preview (zoom), per file.
    pub preview_override: Option<(PathBuf, preview::Key)>,
}

/// The terminal, with a large write buffer so a full frame goes out in one
/// or two `write` calls instead of dozens.
pub type Term =
    ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::BufWriter<std::io::Stdout>>>;

pub struct Options {
    pub entry: Option<PathBuf>,
    pub cwd_file: Option<PathBuf>,
    pub chooser_file: Option<PathBuf>,
}

impl App {
    #[cfg(test)]
    pub fn new(opts: Options) -> Self {
        Self::with_config(opts, crate::config::load_all())
    }

    /// Where to start: the entry directory, or the parent of an entry file
    /// (which is then revealed).
    pub fn resolve_start(entry: Option<&Path>) -> (PathBuf, Option<PathBuf>) {
        let cwd = std::env::current_dir()
            .ok()
            .or_else(|| std::env::var_os("PWD").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("/"));
        match entry {
            Some(e) => {
                let e = crate::util::clean_path(
                    &cwd.join(crate::util::expand_path(&e.to_string_lossy())),
                );
                if e.is_dir() {
                    (e, None)
                } else {
                    let dir = e.parent().map(Path::to_path_buf).unwrap_or(cwd.clone());
                    (dir, Some(e))
                }
            }
            None => (cwd, None),
        }
    }

    pub fn with_config(opts: Options, (cfg, th, km): (Config, Theme, Keymap)) -> Self {
        crate::trace("config loaded");
        let (tx, rx) = channel();

        let mut warnings: Vec<String> = Vec::new();
        warnings.extend(cfg.warnings.iter().cloned());
        warnings.extend(th.warnings.iter().cloned());
        warnings.extend(km.warnings.iter().cloned());

        let pref = Pref {
            sort: cfg.mgr.sort,
            linemode: cfg.mgr.linemode.clone(),
            show_hidden: cfg.mgr.show_hidden,
        };

        let (start, reveal) = Self::resolve_start(opts.entry.as_deref());

        let previewer = Previewer::spawn(tx.clone(), th.syntect_theme.clone());
        let mut app = Self {
            cfg,
            th,
            km,
            tabs: vec![Tab::new(start, pref)],
            cur: 0,
            yanked: Yanked::default(),
            tx,
            rx,
            input_thread: None,
            watcher: None,
            watched: HashSet::new(),
            pending_reload: HashMap::new(),
            loading: HashSet::new(),
            previewer,
            preview_skip: 0,
            preview_for: None,
            preview_key: None,
            preview_content: None,
            picker: None,
            picker_tried: false,
            want_picker: false,
            tasks: Vec::new(),
            next_task: 1,
            next_search: 1,
            mimes: HashMap::new(),
            mime_pending: HashSet::new(),
            dir_sizes: HashMap::new(),
            input: None,
            confirm: None,
            pick: None,
            which: None,
            help: None,
            tasks_view: None,
            spot: None,
            notes: Vec::new(),
            fg: None,
            size: (80, 24),
            areas: Areas::default(),
            pane: [true, true, true],
            quit: false,
            write_cwd: true,
            cwd_file: opts.cwd_file,
            chooser_file: opts.chooser_file,
            chosen: None,
            dirty: true,
            last_title: String::new(),
            pending_hover: HashMap::new(),
            after_fg: None,
            pending_refresh: HashSet::new(),
            painted: false,
            after_plugin: None,
            yank_stamp: None,
            force_clear: false,
            lua: None,
            ps_seen: (PathBuf::new(), None, 0),
            plugin_cache: HashMap::new(),
            fetched: HashSet::new(),
            lua_borders: Vec::new(),
            status_hidden: false,
            preview_override: None,
        };
        if let Some(r) = reveal {
            app.pending_hover.insert(app.tabs[0].cwd.clone(), r);
        }
        for w in warnings {
            app.notify(Level::Warn, "Config", &w);
        }
        app
    }

    // ----------------------------------------------------------------------
    // Accessors
    // ----------------------------------------------------------------------

    pub fn tab(&self) -> &Tab {
        &self.tabs[self.cur]
    }

    pub fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.cur]
    }

    pub fn cwd(&self) -> PathBuf {
        self.tab().cwd.clone()
    }

    pub fn hovered(&self) -> Option<&File> {
        self.tab().hovered()
    }

    pub fn mime(&self, f: &File) -> std::borrow::Cow<'_, str> {
        if let Some(m) = f.quick_mime() {
            return std::borrow::Cow::Borrowed(m);
        }
        match self.mimes.get(&f.url) {
            Some(m) => std::borrow::Cow::Borrowed(m.as_str()),
            None => std::borrow::Cow::Borrowed(""),
        }
    }

    pub fn notify(&mut self, level: Level, title: &str, body: &str) {
        let secs = match level {
            Level::Info => 3,
            Level::Warn => 5,
            Level::Error => 8,
        };
        self.notes.push(Notification {
            level,
            title: title.to_string(),
            body: body.to_string(),
            until: Instant::now() + Duration::from_secs(secs),
        });
        if self.notes.len() > 6 {
            self.notes.remove(0);
        }
        self.dirty = true;
    }

    pub fn running_tasks(&self) -> usize {
        self.tasks
            .iter()
            .filter(|t| t.state == TaskState::Running)
            .count()
    }

    // ----------------------------------------------------------------------
    // Layout
    // ----------------------------------------------------------------------

    pub fn compute_areas(&mut self) {
        let (w, h) = self.size;
        let tabs_h: u16 = if self.tabs.len() > 1 && h > 2 { 1 } else { 0 };
        let mut header = Rect::new(0, 0, w, h.min(1));
        let mut tabs = Rect::new(0, header.height, w, tabs_h);
        let body_y = header.height + tabs_h;
        let status_h = if h > body_y { 1 } else { 0 };
        let mut body = Rect::new(0, body_y, w, h.saturating_sub(body_y + status_h));
        let mut status = Rect::new(0, h.saturating_sub(status_h), w, status_h);
        // A plugin that rearranged yazi's Root layout.
        if let Some(rt) = self.lua.as_ref() {
            if rt.component_active("Root") && rt.root_custom() {
                if let Some([hd, tb, bd, st]) = rt.root_layout(w, h, tabs_h) {
                    (header, tabs, body, status) = (hd, tb, bd, st);
                }
            }
        }
        let mut r = self.cfg.mgr.ratio;
        if !self.pane[0] {
            r[0] = 0;
        }
        if !self.pane[2] {
            r[2] = 0;
        }
        if !self.pane[1] {
            r[1] = 0;
        }
        let cols = split_ratio(body.x, body.width, r);
        let mut c: Vec<Rect> = cols
            .iter()
            .map(|&(x, cw)| Rect::new(x, body.y, cw, body.height))
            .collect();
        // Plugins that wrap yazi's Tab layout (full-border, no-status…).
        self.lua_borders.clear();
        self.status_hidden = false;
        if let Some(rt) = self.lua.as_ref() {
            self.status_hidden = rt.component_active("Status") && rt.status_hidden();
            if rt.component_active("Tab") && rt.tab_custom() {
                if let Some(l) = rt.tab_layout(body) {
                    c = l.chunks.to_vec();
                    self.lua_borders = l.borders;
                }
            }
        }
        let rows = [header, tabs, body, status];
        let chunks = [c[0], c[1], c[2]];
        let p = if chunks[1].width > 0 { 0 } else { 1 };
        let pad = |r: Rect, top: u16, right: u16, bottom: u16, left: u16| -> Rect {
            Rect {
                x: r.x + left.min(r.width),
                y: r.y + top.min(r.height),
                width: r.width.saturating_sub(left + right),
                height: r.height.saturating_sub(top + bottom),
            }
        };
        self.areas = Areas {
            header: rows[0],
            tabs: (tabs_h > 0).then_some(rows[1]),
            chunks,
            parent: pad(chunks[0], 0, p, 0, 1),
            current: pad(chunks[1], 0, 1, 0, 1),
            preview: pad(chunks[2], 0, 1, 0, p),
            status: rows[3],
        };
    }

    // ----------------------------------------------------------------------
    // Loading
    // ----------------------------------------------------------------------

    /// Read `path` on a worker thread (sorted there too).
    pub fn load(&mut self, path: &Path) {
        if !self.loading.insert(path.to_path_buf()) {
            return;
        }
        let tx = self.tx.clone();
        let path = path.to_path_buf();
        let sort = self.tab().pref.sort;
        std::thread::spawn(move || {
            let dir_mtime = std::fs::metadata(&path).ok().map(|m| {
                use std::os::unix::fs::MetadataExt;
                (m.mtime(), m.mtime_nsec() as u32)
            });
            let result = read_dir(&path)
                .map(|mut files| {
                    sort_files(&mut files, sort);
                    (files, dir_mtime)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Loaded { path, result, sort });
        });
    }

    /// Load synchronously (used for the very first paint).
    pub fn load_now(&mut self, path: &Path) {
        let sort = self.tab().pref.sort;
        let dir_mtime = std::fs::metadata(path).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            (m.mtime(), m.mtime_nsec() as u32)
        });
        let result = read_dir(path)
            .map(|mut files| {
                sort_files(&mut files, sort);
                (files, dir_mtime)
            })
            .map_err(|e| e.to_string());
        self.on_loaded(path.to_path_buf(), result, sort);
    }

    fn on_loaded(
        &mut self,
        path: PathBuf,
        result: crate::event::Listing,
        sort: crate::config::SortOpts,
    ) {
        self.loading.remove(&path);
        let hover = self.pending_hover.remove(&path);
        let owners: Vec<usize> = (0..self.tabs.len())
            .filter(|&i| self.tabs[i].folders.contains_key(&path))
            .collect();
        let (mut files, mtime) = match result {
            Ok((f, m)) => (Some(f), m),
            Err(e) => {
                for &ti in &owners {
                    if let Some(f) = self.tabs[ti].folders.get_mut(&path) {
                        f.set_failed(e.clone());
                    }
                }
                self.dirty = true;
                return;
            }
        };
        for (k, &ti) in owners.iter().enumerate() {
            // Move the listing into the last owner; clone only for extra tabs.
            let items = if k + 1 == owners.len() {
                files.take().unwrap_or_default()
            } else {
                files.clone().unwrap_or_default()
            };
            let tab = &mut self.tabs[ti];
            let pref_sort = tab.pref.sort;
            let hidden = tab.pref.show_hidden;
            let cwd = tab.cwd.clone();
            let Some(folder) = tab.folders.get_mut(&path) else {
                continue;
            };
            let first = folder.stage != Stage::Loaded;
            folder.dir_mtime = mtime;
            folder.set_items(items, pref_sort, hidden, pref_sort == sort);
            if first && cwd.parent() == Some(path.as_path()) {
                folder.hover(&cwd);
            }
            if ti == self.cur {
                if let Some(h) = &hover {
                    folder.hover(h);
                }
            }
        }
        self.dirty = true;
    }

    /// Make sure a folder exists in the tab and is fresh, loading if needed.
    fn ensure_loaded(&mut self, path: &Path) {
        let tab = &mut self.tabs[self.cur];
        let folder = tab
            .folders
            .entry(path.to_path_buf())
            .or_insert_with(|| Folder::new(path.to_path_buf()));
        let stale = match folder.stage {
            Stage::Loading => true,
            Stage::Failed(_) => false,
            Stage::Loaded => {
                // Directory mtime changed since we read it → entries changed.
                let now = std::fs::metadata(path).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.mtime(), m.mtime_nsec() as u32)
                });
                now != folder.dir_mtime
            }
        };
        let pref = tab.pref.clone();
        folder.apply_prefs(pref.sort, pref.show_hidden);
        if stale {
            // Fill from another tab's copy while we reload, if there is one.
            if folder.stage == Stage::Loading {
                let src = self
                    .tabs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != self.cur)
                    .find_map(|(_, t)| {
                        t.folders
                            .get(path)
                            .filter(|f| f.stage == Stage::Loaded)
                            .map(|f| f.items.clone())
                    });
                if let Some(items) = src {
                    let tab = &mut self.tabs[self.cur];
                    if let Some(f) = tab.folders.get_mut(path) {
                        f.set_items(items, pref.sort, pref.show_hidden, false);
                    }
                }
            }
            self.load(path);
        }
    }

    // ----------------------------------------------------------------------
    // Sync: derived state after every batch of messages
    // ----------------------------------------------------------------------

    pub fn sync(&mut self) {
        self.compute_areas();
        let cwd = self.cwd();
        if self.tab().search.is_none() {
            self.ensure_loaded(&cwd);
        }
        if let Some(p) = self.tab().parent_path() {
            self.ensure_loaded(&p);
            let cwd2 = cwd.clone();
            let tab = &mut self.tabs[self.cur];
            if let Some(pf) = tab.folders.get_mut(&p) {
                pf.hover(&cwd2);
            }
        }
        let hovered_dir = self.tab().hovered_dir();
        if let Some(d) = &hovered_dir {
            if self.pane[2] {
                self.ensure_loaded(d);
            }
        }

        // Scroll positions.
        let so = self.cfg.mgr.scrolloff;
        let (ph, ch, vh) = (
            self.areas.parent.height as usize,
            self.areas.current.height as usize,
            self.areas.preview.height as usize,
        );
        let parent = self.tab().parent_path();
        let tab = &mut self.tabs[self.cur];
        tab.current_mut().scroll_into_view(ch, so);
        if let Some(p) = parent {
            if let Some(f) = tab.folders.get_mut(&p) {
                f.scroll_into_view(ph, so);
            }
        }
        if let Some(d) = &hovered_dir {
            if let Some(f) = tab.folders.get_mut(d) {
                f.scroll_into_view(vh, so);
            }
        }

        // Long sessions visit many folders: forget the ones far away.
        let keep: Vec<PathBuf> = hovered_dir.iter().cloned().collect();
        self.tabs[self.cur].trim_cache(&keep);

        // Before the first frame only what is needed to paint it; previews,
        // MIME sniffing and the file watcher start right after.
        if self.painted {
            self.sync_preview();
            self.sync_mimes();
            self.sync_watch();
            self.sync_ps();
            self.sync_fetchers();
        }
        self.sync_title();
    }

    fn sync_title(&mut self) {
        let t = self
            .cfg
            .mgr
            .title_format
            .replace("{cwd}", &crate::util::readable_path(&self.cwd()));
        if t != self.last_title && !t.is_empty() && !cfg!(test) {
            let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle(&t));
            self.last_title = t;
        }
    }

    fn preview_request(&self, f: &File, skip: usize) -> preview::Request {
        let a = self.areas.preview;
        preview::Request {
            key: preview::Key {
                url: f.url.clone(),
                mtime: f.cha.mtime,
                len: f.cha.len,
                skip,
                w: a.width,
                h: a.height,
            },
            previewer: self
                .cfg
                .previewer_for(&f.url, f.is_dir(), &self.mime(f))
                .to_string(),
            mime: self.mime(f).into_owned(),
            picker: self.picker.clone(),
            tab_size: self.cfg.preview.tab_size,
            max_width: self.cfg.preview.max_width,
            max_height: self.cfg.preview.max_height,
            quality: self.cfg.preview.image_quality,
        }
    }

    fn needs_picker(mime: &str) -> bool {
        mime.starts_with("image/")
            || mime.starts_with("video/")
            || mime.starts_with("font/")
            || mime == "application/pdf"
    }

    fn sync_preview(&mut self) {
        let hovered = self.hovered().cloned();
        let Some(f) = hovered else {
            self.preview_key = None;
            self.preview_content = None;
            return;
        };
        if let Some((u, k)) = &self.preview_override {
            if *u == f.url {
                if self.preview_key.as_ref() != Some(k) {
                    self.preview_key = Some(k.clone());
                    self.preview_content = self.previewer.get(k);
                }
                return;
            }
            self.preview_override = None;
        }
        if self.preview_for.as_deref() != Some(&f.url) {
            self.preview_for = Some(f.url.clone());
            self.preview_skip = 0;
        }
        if f.is_dir() || self.areas.preview.width == 0 || self.areas.preview.height == 0 {
            self.preview_key = None;
            self.preview_content = None;
            return;
        }
        let mime = self.mime(&f).into_owned();
        if mime.is_empty() {
            // Waiting for the content sniffer.
            self.preview_key = None;
            self.preview_content = None;
            return;
        }
        if Self::needs_picker(&mime) && !self.picker_tried {
            self.want_picker = true;
            return;
        }
        let req = self.preview_request(&f, self.preview_skip);
        let key = req.key.clone();
        if self.preview_key.as_ref() != Some(&key) {
            self.preview_key = Some(key.clone());
            match self.previewer.get(&key) {
                Some(c) => {
                    self.preview_content = Some(c);
                }
                None => {
                    // Keep showing the old content only if it is the same
                    // file (scrolling); otherwise clear to avoid confusion.
                    let same = self.preview_content.is_some()
                        && self.preview_key.as_ref().map(|k| &k.url) == Some(&f.url);
                    if !same {
                        self.preview_content = None;
                    }
                    self.previewer.request(req);
                }
            }
        }
        // Prefetch neighbours so moving the cursor is instant.
        let folder = self.tab().current();
        let i = folder.cursor;
        let mut pre = Vec::new();
        for j in [i + 1, i.wrapping_sub(1), i + 2] {
            if let Some(n) = folder.get(j) {
                if !n.is_dir() {
                    let m = self.mime(n);
                    if !m.is_empty() && (!Self::needs_picker(&m) || self.picker.is_some()) {
                        pre.push(self.preview_request(n, 0));
                    }
                }
            }
        }
        self.previewer.prefetch(pre);
    }

    /// Sniff MIME types of visible files whose extension says nothing.
    fn sync_mimes(&mut self) {
        let folder = self.tab().current();
        let win = folder.window(self.areas.current.height as usize);
        let mut todo = Vec::new();
        for i in win {
            if let Some(f) = folder.get(i) {
                if f.quick_mime().is_none()
                    && !self.mimes.contains_key(&f.url)
                    && !self.mime_pending.contains(&f.url)
                {
                    todo.push(f.url.clone());
                }
            }
        }
        if todo.is_empty() {
            return;
        }
        for p in &todo {
            self.mime_pending.insert(p.clone());
        }
        let tx = self.tx.clone();
        rayon::spawn(move || {
            let out: Vec<(PathBuf, String)> = todo
                .into_iter()
                .map(|p| {
                    let m = crate::fs::sniff_mime(&p);
                    (p, m)
                })
                .collect();
            let _ = tx.send(Msg::Mimes(out));
        });
    }

    fn sync_watch(&mut self) {
        let mut want: HashSet<PathBuf> = HashSet::new();
        for t in &self.tabs {
            want.insert(t.cwd.clone());
            if let Some(p) = t.cwd.parent() {
                want.insert(p.to_path_buf());
            }
        }
        if let Some(d) = self.tab().hovered_dir() {
            want.insert(d);
        }
        if want == self.watched {
            return;
        }
        if self.watcher.is_none() {
            let tx = self.tx.clone();
            self.watcher =
                notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        if matches!(ev.kind, notify::EventKind::Access(_)) {
                            return;
                        }
                        let _ = tx.send(Msg::Watch(ev.paths));
                    }
                })
                .ok();
        }
        let Some(w) = self.watcher.as_mut() else {
            return;
        };
        for p in self.watched.difference(&want) {
            let _ = w.unwatch(p);
        }
        for p in want.difference(&self.watched) {
            let _ = w.watch(p, notify::RecursiveMode::NonRecursive);
        }
        self.watched = want;
    }

    // ----------------------------------------------------------------------
    // Messages
    // ----------------------------------------------------------------------

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Input(ev) => self.on_event(ev),
            Msg::Loaded { path, result, sort } => self.on_loaded(path, result, sort),
            Msg::Preview { key, content } => {
                let c = self.previewer.insert(key.clone(), content);
                if self.preview_key.as_ref() == Some(&key) {
                    // Text previews may clamp the scroll offset.
                    if let PreviewContent::Text { skip, .. } = &*c {
                        if *skip != key.skip {
                            self.preview_skip = *skip;
                        }
                    }
                    self.preview_content = Some(c);
                    self.dirty = true;
                }
            }
            Msg::Watch(paths) => {
                let at = Instant::now() + Duration::from_millis(40);
                for p in paths {
                    self.previewer.invalidate(&p);
                    for cand in [p.parent().map(Path::to_path_buf), Some(p.clone())]
                        .into_iter()
                        .flatten()
                    {
                        if self.watched.contains(&cand) {
                            self.pending_reload.entry(cand).or_insert(at);
                        }
                    }
                }
            }
            Msg::Task(ev) => self.on_task(ev),
            Msg::Search {
                id,
                files,
                done,
                error,
            } => {
                for t in &mut self.tabs {
                    let Some(s) = t.search.as_mut() else { continue };
                    if s.id != id {
                        continue;
                    }
                    if !files.is_empty() {
                        let mut items = std::mem::take(&mut s.folder.items);
                        items.extend(files.iter().cloned());
                        let sort = t.pref.sort;
                        let hidden = true;
                        s.folder.set_items(items, sort, hidden, false);
                    }
                    if done {
                        s.done = true;
                        if s.folder.stage == Stage::Loading {
                            s.folder.set_items(Vec::new(), t.pref.sort, true, true);
                        }
                    }
                }
                if let Some(e) = error {
                    self.notify(Level::Warn, "Search", &e);
                }
                self.dirty = true;
            }
            Msg::Mimes(list) => {
                for (p, m) in list {
                    self.mime_pending.remove(&p);
                    self.mimes.insert(p, m);
                }
                self.dirty = true;
            }
            Msg::Remote(line) => {
                if let Some(c) = Cmd::parse(&line) {
                    self.run(Layer::Mgr, std::slice::from_ref(&c));
                }
                self.dirty = true;
            }
            Msg::DirSize { path, size, items } => {
                self.dir_sizes.insert(path.clone(), (size, items));
                if let Some(s) = &mut self.spot {
                    if s.url == path {
                        // The first `Size` row is the Folder section's.
                        if let Some(r) = s.rows.iter_mut().find(|r| r.0 == "  Size:") {
                            r.1 = crate::util::readable_size(size);
                        }
                        let _ = items;
                    }
                }
                self.dirty = true;
            }
        }
    }

    fn on_task(&mut self, ev: TaskEvent) {
        match ev {
            TaskEvent::Progress {
                id,
                bytes_done,
                bytes_total,
                items_done,
                items_total,
            } => {
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.bytes_done = bytes_done;
                    t.bytes_total = bytes_total;
                    t.items_done = items_done;
                    t.items_total = items_total;
                }
            }
            TaskEvent::Log { id, line } => {
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.log.push(line);
                    if t.log.len() > 2000 {
                        t.log.remove(0);
                    }
                }
            }
            TaskEvent::Finished {
                id,
                result,
                touched,
                cancelled,
            } => {
                let mut note = None;
                if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
                    t.state = match (&result, cancelled) {
                        (_, true) => TaskState::Cancelled,
                        (Ok(()), _) => TaskState::Done,
                        (Err(e), _) => {
                            note = Some((t.title.clone(), e.clone()));
                            TaskState::Failed(e.clone())
                        }
                    };
                }
                if let Some((title, e)) = note {
                    self.notify(Level::Error, &title, &e);
                }
                for d in touched {
                    self.refresh_dir(&d);
                }
                // Keep the list short: drop finished tasks beyond the last 30.
                let finished: Vec<u64> = self
                    .tasks
                    .iter()
                    .filter(|t| t.state != TaskState::Running)
                    .map(|t| t.id)
                    .collect();
                if finished.len() > 30 {
                    let drop: HashSet<u64> =
                        finished[..finished.len() - 30].iter().copied().collect();
                    self.tasks.retain(|t| !drop.contains(&t.id));
                }
            }
        }
        self.dirty = true;
    }

    /// Reload a directory everywhere it is shown.
    pub fn refresh_dir(&mut self, d: &Path) {
        let shown = self.tabs.iter().any(|t| t.folders.contains_key(d));
        if shown {
            self.load(d);
        }
    }

    pub fn spawn_task(&mut self, job: crate::tasks::Job) {
        let id = self.next_task;
        self.next_task += 1;
        let info = crate::tasks::spawn(id, job, self.tx.clone());
        self.tasks.push(info);
        self.dirty = true;
    }

    // ----------------------------------------------------------------------
    // Input events
    // ----------------------------------------------------------------------

    pub fn active_layer(&self) -> Layer {
        if self.help.is_some() {
            if let Some(i) = &self.input {
                if i.kind == InputKind::HelpFilter {
                    return Layer::Input;
                }
            }
            return Layer::Help;
        }
        if self.input.is_some() {
            return Layer::Input;
        }
        if self.confirm.is_some() {
            return Layer::Confirm;
        }
        if self.pick.is_some() {
            return Layer::Pick;
        }
        if self.spot.is_some() {
            return Layer::Spot;
        }
        if self.tasks_view.is_some() {
            return Layer::Tasks;
        }
        Layer::Mgr
    }

    fn on_event(&mut self, ev: Event) {
        self.dirty = true;
        match ev {
            Event::Key(k) => {
                if k.kind == KeyEventKind::Release {
                    return;
                }
                self.on_key(Key::from_event(k));
            }
            Event::Paste(s) => {
                if let Some(i) = &mut self.input {
                    let s = s.replace(['\n', '\r'], " ");
                    i.input.type_str(&s);
                    self.on_input_change();
                }
            }
            Event::Resize(w, h) => {
                self.size = (w, h);
                self.preview_key = None;
            }
            Event::Mouse(m) if self.cfg.mgr.mouse => self.on_mouse(m),
            _ => {}
        }
    }

    pub fn on_key(&mut self, key: Key) {
        // A chord in progress.
        if let Some(w) = self.which.as_mut() {
            let ctrl_bracket = key.ctrl && key.code == crossterm::event::KeyCode::Char('[');
            if key.code == crossterm::event::KeyCode::Esc || ctrl_bracket {
                let plugin = w.plugin;
                self.which = None;
                if plugin {
                    self.plugin_resume(crate::plugin::Resume::Index(None));
                }
                return;
            }
            if w.plugin {
                let depth = w.keys.len();
                let hit = w
                    .cands
                    .iter()
                    .position(|b| b.on.get(depth) == Some(&key) && b.on.len() == depth + 1);
                self.which = None;
                self.plugin_resume(crate::plugin::Resume::Index(hit.map(|i| i + 1)));
                return;
            }
            w.keys.push(key);
            let layer = w.layer;
            let keys = w.keys.clone();
            let found = match self.km.lookup(layer, &keys) {
                Lookup::Run(b) => Some(Ok(b.run.clone())),
                Lookup::Prefix(c) => Some(Err(c.into_iter().cloned().collect::<Vec<_>>())),
                Lookup::None => None,
            };
            match found {
                Some(Ok(run)) => {
                    self.which = None;
                    self.run(layer, &run);
                }
                Some(Err(cands)) => {
                    if let Some(w) = self.which.as_mut() {
                        w.cands = cands;
                    }
                }
                None => self.which = None,
            }
            return;
        }

        let layer = self.active_layer();

        // Completion menu gets first pick of keys while it is open.
        if layer == Layer::Input {
            let cmp_open = self
                .input
                .as_ref()
                .is_some_and(|i| i.cmp_shown && !i.cmp.is_empty());
            if cmp_open {
                if let Lookup::Run(b) = self.km.lookup(Layer::Cmp, &[key]) {
                    let run = b.run.clone();
                    self.run(Layer::Cmp, &run);
                    return;
                }
            }
            // Typing in insert mode beats the normal-mode bindings.
            let typing = self
                .input
                .as_ref()
                .is_some_and(|i| matches!(i.input.mode, InputMode::Insert | InputMode::Replace));
            if typing {
                if let Some(c) = key.typed_char() {
                    if let Some(i) = self.input.as_mut() {
                        i.input.type_char(c);
                    }
                    self.on_input_change();
                    return;
                }
            }
        }
        if layer == Layer::Help {
            if let Some(c) = key.typed_char() {
                let _ = c;
            }
        }

        match self.km.lookup(layer, &[key]) {
            Lookup::Run(b) => {
                let run = b.run.clone();
                self.run(layer, &run);
            }
            Lookup::Prefix(c) => {
                self.which = Some(WhichState {
                    layer,
                    keys: vec![key],
                    cands: c.into_iter().cloned().collect(),
                    plugin: false,
                    silent: false,
                });
            }
            Lookup::None => {}
        }
    }

    /// Run commands from a binding, routing `layer:cmd` prefixes.
    pub fn run(&mut self, layer: Layer, cmds: &[Cmd]) {
        for c in cmds {
            let (l, _) = c.layer();
            let target = match l {
                Some("mgr" | "manager") => Layer::Mgr,
                Some("tasks") => Layer::Tasks,
                Some("spot") => Layer::Spot,
                Some("pick") => Layer::Pick,
                Some("input") => Layer::Input,
                Some("confirm") => Layer::Confirm,
                Some("cmp") => Layer::Cmp,
                Some("help") => Layer::Help,
                Some("app") => Layer::Mgr,
                _ => layer,
            };
            self.exec(target, c);
            if self.quit {
                return;
            }
        }
    }

    fn on_mouse(&mut self, m: crossterm::event::MouseEvent) {
        let (x, y) = (m.column, m.row);
        let inside = |r: Rect| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height;
        if self.active_layer() != Layer::Mgr {
            return;
        }
        let a = self.areas;
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let step: isize = if m.kind == MouseEventKind::ScrollDown {
                    1
                } else {
                    -1
                };
                if inside(a.preview) {
                    self.exec(
                        Layer::Mgr,
                        &Cmd::parse(&format!("seek {}", step * 5)).unwrap(),
                    );
                } else if inside(a.current) {
                    self.exec(Layer::Mgr, &Cmd::parse(&format!("arrow {step}")).unwrap());
                }
            }
            MouseEventKind::Down(btn) => {
                if let Some(t) = a.tabs {
                    if inside(t) {
                        // Tabs are laid out left to right with equal max width.
                        let n = self.tabs.len().max(1) as u16;
                        let i = ((x - t.x) / (t.width / n).max(1)) as usize;
                        self.exec(Layer::Mgr, &Cmd::parse(&format!("tab_switch {i}")).unwrap());
                        return;
                    }
                }
                if inside(a.current) {
                    let row = (y - a.current.y) as usize;
                    let folder = self.tab().current();
                    let idx = folder.offset + row;
                    if idx < folder.len() {
                        self.tab_mut().current_mut().arrow_to(idx);
                        if btn == MouseButton::Right {
                            self.exec(Layer::Mgr, &Cmd::new("open"));
                        }
                    }
                } else if inside(a.parent) {
                    let row = (y - a.parent.y) as usize;
                    let target = self.tab().parent().and_then(|p| {
                        let i = p.offset + row;
                        p.get(i).map(|f| f.url.clone())
                    });
                    match target {
                        Some(t) => self.reveal(&t),
                        None => self.exec(Layer::Mgr, &Cmd::new("leave")),
                    }
                } else if inside(a.preview) {
                    if let Some(d) = self.tab().hovered_dir() {
                        let row = (y - a.preview.y) as usize;
                        let target = self
                            .tab()
                            .folders
                            .get(&d)
                            .and_then(|f| f.get(f.offset + row).map(|x| x.url.clone()));
                        if let Some(t) = target {
                            self.reveal(&t);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ----------------------------------------------------------------------
    // Terminal hand-off
    // ----------------------------------------------------------------------

    fn take_picker(&mut self) {
        self.want_picker = false;
        self.picker_tried = true;
        // Fast path: terminals we can identify need no round trip.
        let term = std::env::var("TERM").unwrap_or_default();
        let prog = std::env::var("TERM_PROGRAM").unwrap_or_default();
        let kitty = std::env::var_os("KITTY_WINDOW_ID").is_some()
            || term.contains("kitty")
            || prog.eq_ignore_ascii_case("ghostty")
            || term.contains("ghostty");
        let in_tmux = std::env::var_os("TMUX").is_some();
        if kitty && !in_tmux {
            if let Ok(ws) = crossterm::terminal::window_size() {
                if ws.width > 0 && ws.columns > 0 && ws.rows > 0 {
                    let fs = (ws.width / ws.columns, ws.height / ws.rows);
                    #[allow(deprecated)] // the documented way to skip the query
                    let mut p = Picker::from_fontsize(ratatui_image::FontSize::new(fs.0, fs.1));
                    p.set_protocol_type(ProtocolType::Kitty);
                    self.picker = Some(p);
                    return;
                }
            }
        }
        if let Some(it) = &self.input_thread {
            it.pause();
        }
        self.picker = Picker::from_query_stdio().ok();
        if let Some(it) = &self.input_thread {
            it.resume();
        }
        // Terminals that do not understand a query may echo it: repaint all.
        self.force_clear = true;
        if self.picker.is_none() {
            self.picker = Some(Picker::halfblocks());
        }
    }

    fn run_fg(&mut self, terminal: &mut Term, fg: Fg) {
        if let Some(it) = &self.input_thread {
            it.pause();
        }
        restore_terminal(self.cfg.mgr.mouse);
        let cwd = self.cwd();
        match fg {
            Fg::PluginHide => {
                // Run the plugin with the terminal handed over until it
                // shows the UI again, finishes, or needs a popup.
                if let Some(rt) = self.lua.as_mut() {
                    rt.set_hidden(true);
                }
                let mut step = crate::plugin::Step::Hide;
                while matches!(step, crate::plugin::Step::Hide) {
                    let snap = self.plugin_snapshot();
                    step = match self.lua.as_mut() {
                        Some(rt) => rt.resume(crate::plugin::Resume::Nothing, &snap),
                        None => crate::plugin::Step::Done,
                    };
                }
                if let Some(rt) = self.lua.as_mut() {
                    rt.set_hidden(false);
                }
                self.after_plugin = Some(step);
            }
            Fg::Suspend => {
                // SAFETY: raising SIGTSTP stops us until the shell resumes us.
                unsafe {
                    libc::raise(libc::SIGTSTP);
                }
            }
            Fg::Shell {
                script,
                args,
                cwd,
                wait_key,
            } => {
                let status = std::process::Command::new("sh")
                    .arg("-c")
                    .arg(&script)
                    .args(&args)
                    .current_dir(&cwd)
                    .status();
                if let Err(e) = status {
                    eprintln!("wisp: {e}");
                }
                if wait_key {
                    print!("\nPress Enter to return...");
                    let _ = std::io::stdout().flush();
                    let mut s = String::new();
                    let _ = std::io::stdin().read_line(&mut s);
                }
            }
            Fg::Fzf => {
                let out = std::process::Command::new("fzf")
                    .current_dir(&cwd)
                    .stdin(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::piped())
                    .output();
                match out {
                    Ok(o) => {
                        let s = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
                        if !s.is_empty() {
                            let p = crate::util::clean_path(&cwd.join(s));
                            self.after_fg = Some(AfterFg::Reveal(p));
                        }
                    }
                    Err(e) => self.after_fg = Some(AfterFg::Error(format!("fzf: {e}"))),
                }
            }
            Fg::Zoxide => {
                let out = std::process::Command::new("zoxide")
                    .args(["query", "-i", "--exclude"])
                    .arg(&cwd)
                    .current_dir(&cwd)
                    .stdin(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::piped())
                    .output();
                match out {
                    Ok(o) => {
                        let s = String::from_utf8_lossy(&o.stdout).trim_end().to_string();
                        if !s.is_empty() {
                            self.after_fg = Some(AfterFg::Cd(PathBuf::from(s)));
                        }
                    }
                    Err(e) => self.after_fg = Some(AfterFg::Error(format!("zoxide: {e}"))),
                }
            }
            Fg::BulkRename(paths) => {
                let r = crate::commands::bulk_rename(&paths);
                self.after_fg = Some(match r {
                    Ok(n) => AfterFg::Info(format!("Renamed {n} file(s)")),
                    Err(e) => AfterFg::Error(e),
                });
                for d in paths.iter().filter_map(|p| p.parent()) {
                    self.pending_refresh.insert(d.to_path_buf());
                }
            }
        }
        init_terminal(self.cfg.mgr.mouse);
        let _ = terminal.clear();
        if let Some(it) = &self.input_thread {
            it.resume();
        }
        self.last_title.clear();
        self.preview_key = None;
        self.dirty = true;
        if let Some(step) = self.after_plugin.take() {
            match step {
                // The plugin asked to show the UI again: keep it going.
                crate::plugin::Step::Show => self.plugin_resume(crate::plugin::Resume::Nothing),
                other => self.plugin_apply(other),
            }
        }
        match self.after_fg.take() {
            Some(AfterFg::Reveal(p)) => self.reveal(&p),
            Some(AfterFg::Cd(p)) => self.cd(p),
            Some(AfterFg::Error(e)) => self.notify(Level::Error, "Error", &e),
            Some(AfterFg::Info(e)) => self.notify(Level::Info, "Done", &e),
            None => {}
        }
        for d in std::mem::take(&mut self.pending_refresh) {
            self.refresh_dir(&d);
        }
        // Whatever ran may have changed the directory.
        let cwd = self.cwd();
        self.refresh_dir(&cwd);
    }

    // ----------------------------------------------------------------------
    // Main loop
    // ----------------------------------------------------------------------

    pub fn run_loop(
        &mut self,
        terminal: &mut Term,
        prefetched: Option<(PathBuf, std::io::Result<Vec<File>>)>,
    ) -> Result<()> {
        self.size = terminal
            .size()
            .map(|s| (s.width, s.height))
            .unwrap_or((80, 24));
        crate::trace("size");
        self.compute_areas();
        crate::trace("areas");
        // First paint: the cwd listing (usually already read on a helper
        // thread during start-up), everything else in the background.
        let cwd = self.cwd();
        match prefetched {
            Some((p, Ok(files))) if p == cwd => {
                let mtime = std::fs::metadata(&cwd).ok().map(|m| {
                    use std::os::unix::fs::MetadataExt;
                    (m.mtime(), m.mtime_nsec() as u32)
                });
                // A sort that can never match forces `set_items` to sort.
                let mut unsorted = self.tab().pref.sort;
                unsorted.reverse = !unsorted.reverse;
                self.on_loaded(cwd.clone(), Ok((files, mtime)), unsorted);
            }
            _ => self.load_now(&cwd),
        }
        crate::trace("cwd read");
        self.input_thread = Some(InputThread::spawn(self.tx.clone()));
        let mut first = true;
        loop {
            self.sync();
            if first {
                crate::trace("synced");
            }
            if self.want_picker {
                self.take_picker();
                self.sync();
            }
            if std::mem::take(&mut self.force_clear) {
                let _ = terminal.clear();
                self.dirty = true;
            }
            if self.dirty {
                self.dirty = false;
                // Synchronized update: the terminal shows the frame at once.
                let _ = crossterm::queue!(
                    terminal.backend_mut(),
                    crossterm::terminal::BeginSynchronizedUpdate
                );
                terminal.draw(|f| crate::ui::draw(f, self))?;
                let _ = crossterm::execute!(
                    terminal.backend_mut(),
                    crossterm::terminal::EndSynchronizedUpdate
                );
                if first {
                    crate::trace("first draw");
                    first = false;
                    self.painted = true;
                    self.dirty = true;
                    self.run_init_lua();
                    crate::remote::listen(self.tx.clone());
                    continue;
                }
            }
            if let Some(fg) = self.fg.take() {
                self.run_fg(terminal, fg);
                continue;
            }
            if self.quit {
                break;
            }
            let timeout = self.next_deadline();
            match self.rx.recv_timeout(timeout) {
                Ok(m) => self.handle(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Drain whatever else is ready before repainting once.
            let start = Instant::now();
            while let Ok(m) = self.rx.try_recv() {
                self.handle(m);
                if self.quit || start.elapsed() > Duration::from_millis(30) {
                    break;
                }
            }
            self.tick();
        }
        Ok(())
    }

    fn next_deadline(&self) -> Duration {
        let now = Instant::now();
        let mut d = Duration::from_secs(3600);
        for n in &self.notes {
            d = d.min(n.until.saturating_duration_since(now));
        }
        for t in self.pending_reload.values() {
            d = d.min(t.saturating_duration_since(now));
        }
        if self.running_tasks() > 0 {
            d = d.min(Duration::from_millis(250));
        }
        d.max(Duration::from_millis(1))
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let before = self.notes.len();
        self.notes.retain(|n| n.until > now);
        if self.notes.len() != before {
            self.dirty = true;
        }
        let due: Vec<PathBuf> = self
            .pending_reload
            .iter()
            .filter(|(_, t)| **t <= now)
            .map(|(p, _)| p.clone())
            .collect();
        for p in due {
            self.pending_reload.remove(&p);
            self.refresh_dir(&p);
        }
        if self.running_tasks() > 0 {
            self.dirty = true;
        }
    }

    /// Write the cwd/chooser files on exit.
    pub fn finish(&self) {
        crate::remote::cleanup();
        if let (Some(f), Some(chosen)) = (&self.chooser_file, &self.chosen) {
            let s: String = chosen
                .iter()
                .map(|p| format!("{}\n", p.display()))
                .collect();
            let _ = std::fs::write(f, s);
        }
        if self.write_cwd {
            if let Some(f) = &self.cwd_file {
                let _ = std::fs::write(f, self.cwd().as_os_str().as_encoded_bytes());
            }
        }
    }
}

// --------------------------------------------------------------------------
// Lua plugins
// --------------------------------------------------------------------------

impl App {
    /// The `cx` a plugin sees.
    pub fn plugin_snapshot(&self) -> crate::plugin::Snapshot {
        self.snapshot(true)
    }

    /// `cx` for rendering Lua components: only visible files, so it stays
    /// cheap in huge directories.
    pub fn plugin_snapshot_light(&self) -> crate::plugin::Snapshot {
        self.snapshot(false)
    }

    fn snapshot(&self, full: bool) -> crate::plugin::Snapshot {
        use crate::plugin::{FolderSnap, Snapshot, TabSnap};
        let h = self.areas.current.height as usize;
        let folder = |f: &Folder, all: bool, height: usize| FolderSnap {
            cwd: PathBuf::new(),
            files: if all {
                f.files().cloned().collect()
            } else {
                f.window(height).filter_map(|i| f.get(i).cloned()).collect()
            },
            cursor: f.cursor,
            offset: f.offset,
            window: if all {
                f.window(height)
            } else {
                0..f.window(height).len()
            },
            hovered: if all {
                (!f.is_empty()).then_some(f.cursor)
            } else {
                f.cursor.checked_sub(f.offset)
            },
        };
        let tabs = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let all = full && i == self.cur;
                let mut current = folder(t.current(), all, h);
                current.cwd = t.cwd.clone();
                let parent = t.parent().map(|p| {
                    let mut s = folder(p, false, self.areas.parent.height as usize);
                    s.cwd = t.cwd.parent().map(Path::to_path_buf).unwrap_or_default();
                    s
                });
                let preview = t.hovered_dir().and_then(|d| {
                    t.folders.get(&d).map(|f| {
                        let mut s = folder(f, false, self.areas.preview.height as usize);
                        s.cwd = d.clone();
                        s
                    })
                });
                TabSnap {
                    name: t.title(),
                    cwd: t.cwd.clone(),
                    current,
                    parent,
                    preview,
                    preview_skip: self.preview_skip,
                    selected: t.selected.iter().cloned().collect(),
                    mode: match t.mode {
                        crate::tab::Mode::Normal => "normal",
                        crate::tab::Mode::Select(_) => "select",
                        crate::tab::Mode::Unset(_) => "unset",
                    },
                    visual_start: match t.mode {
                        crate::tab::Mode::Select(a) | crate::tab::Mode::Unset(a) => a,
                        crate::tab::Mode::Normal => 0,
                    },
                    sort_by: format!("{:?}", t.pref.sort.by).to_lowercase(),
                    sort_reverse: t.pref.sort.reverse,
                    sort_dir_first: t.pref.sort.dir_first,
                    sort_sensitive: t.pref.sort.sensitive,
                    linemode: t.pref.linemode.clone(),
                    show_hidden: t.pref.show_hidden,
                }
            })
            .collect();
        Snapshot {
            areas: vec![
                ("preview", self.areas.preview),
                ("current", self.areas.current),
                ("parent", self.areas.parent),
            ],
            tabs,
            cur: self.cur,
            yanked: self.yanked.paths.iter().cloned().collect(),
            cut: self.yanked.cut,
        }
    }

    pub(crate) fn ensure_lua(&mut self) -> bool {
        if self.lua.is_none() {
            match crate::plugin::Runtime::new(&self.cfg, &self.th) {
                Ok(rt) => self.lua = Some(rt),
                Err(e) => {
                    self.notify(Level::Error, "Plugin", &e.to_string());
                    return false;
                }
            }
        }
        true
    }

    /// `plugin <name> args…` for an installed Lua plugin.
    pub fn plugin_start(&mut self, name: &str, cmd: &Cmd) {
        if !self.ensure_lua() {
            return;
        }
        let snap = self.plugin_snapshot();
        let step = match self.lua.as_mut() {
            Some(rt) => rt.start(name, cmd, &snap),
            None => return,
        };
        self.plugin_apply(step);
    }

    pub fn plugin_resume(&mut self, how: crate::plugin::Resume) {
        let snap = self.plugin_snapshot();
        let step = match self.lua.as_mut() {
            Some(rt) => rt.resume(how, &snap),
            None => return,
        };
        self.plugin_apply(step);
    }

    /// Act on what a plugin step asked for, then run what it emitted.
    pub fn plugin_apply(&mut self, step: crate::plugin::Step) {
        use crate::plugin::Step;
        let (emits, notes) = match &self.lua {
            Some(rt) => rt.drain(),
            None => (Vec::new(), Vec::new()),
        };
        for (level, title, body) in notes {
            self.notify(level, &title, &body);
        }
        if let Some(r) = self.lua.as_ref().and_then(|rt| rt.ratio()) {
            if r != self.cfg.mgr.ratio {
                self.cfg.mgr.ratio = r;
                self.preview_key = None;
            }
        }
        // A realtime prompt stays open only while the plugin keeps reading it.
        let realtime_open = matches!(
            self.input.as_ref().map(|i| &i.kind),
            Some(InputKind::PluginRealtime)
        );
        if realtime_open && !matches!(step, Step::InputRecv { .. }) {
            self.input = None;
        }
        match step {
            Step::Done | Step::Show => {}
            Step::InputRecv {
                title,
                value,
                obscure,
                open,
            } => {
                if open || !realtime_open {
                    let mut input = Input::new(&value, InputMode::Insert);
                    input.obscure = obscure;
                    self.input = Some(InputState {
                        kind: InputKind::PluginRealtime,
                        input,
                        title,
                        origin: Origin::TopCenter,
                        cmp: Vec::new(),
                        cmp_cursor: 0,
                        cmp_shown: false,
                    });
                }
            }
            Step::Error(e) => self.notify(Level::Error, "Plugin", &e),
            Step::Input {
                title,
                value,
                obscure,
            } => {
                let mut input = Input::new(&value, InputMode::Insert);
                input.obscure = obscure;
                self.input = Some(InputState {
                    kind: InputKind::Plugin,
                    input,
                    title,
                    origin: Origin::TopCenter,
                    cmp: Vec::new(),
                    cmp_cursor: 0,
                    cmp_shown: false,
                });
            }
            Step::Confirm { title, body } => {
                self.confirm = Some(ConfirmState {
                    kind: ConfirmKind::Plugin,
                    title,
                    body,
                    list: Vec::new(),
                    offset: 0,
                });
            }
            Step::Which(cands, silent) => {
                let cands = cands
                    .into_iter()
                    .map(|(on, desc)| Binding {
                        on,
                        run: Vec::new(),
                        desc,
                    })
                    .collect();
                self.which = Some(WhichState {
                    layer: Layer::Mgr,
                    keys: Vec::new(),
                    cands,
                    plugin: true,
                    silent,
                });
            }
            Step::Hide => self.fg = Some(Fg::PluginHide),
        }
        for c in emits {
            self.run(Layer::Mgr, std::slice::from_ref(&c));
        }
        if let Some(rt) = self.lua.as_ref() {
            rt.take_render();
        }
        self.dirty = true;
    }

    /// Run the user's `init.lua` (once, after the first frame).
    fn run_init_lua(&mut self) {
        // The preview thread runs Lua previewers in its own state.
        crate::preview::set_lua_env(self.cfg.clone(), self.th.clone());
        let Some(path) = crate::plugin::find_init() else {
            return;
        };
        if !self.ensure_lua() {
            return;
        }
        let r = self.lua.as_mut().map(|rt| rt.run_init(&path));
        if let Some(Err(e)) = r {
            self.notify(Level::Error, "init.lua", &e);
        }
        let (emits, notes) = self.lua.as_ref().map(|rt| rt.drain()).unwrap_or_default();
        for (level, title, body) in notes {
            self.notify(level, &title, &body);
        }
        for c in emits {
            self.run(Layer::Mgr, std::slice::from_ref(&c));
        }
    }

    /// Show `image` in the preview of the hovered file (from `ya.image_show`).
    pub fn show_image(&mut self, image: &Path) {
        let Some(f) = self.hovered().cloned() else {
            return;
        };
        if self.picker.is_none() {
            self.want_picker = true;
            return;
        }
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut req = self.preview_request(&f, 0);
        req.previewer = format!("@image {}", image.display());
        req.key.skip = usize::MAX - N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = req.key.clone();
        self.preview_override = Some((f.url.clone(), key.clone()));
        self.preview_key = Some(key);
        self.preview_content = None;
        self.previewer.request(req);
    }

    /// Whether `name` is an installed Lua plugin (cached).
    pub fn is_plugin(&mut self, name: &str) -> bool {
        if let Some(v) = self.plugin_cache.get(name) {
            return *v;
        }
        let v = crate::plugin::find_plugin(name).is_some();
        self.plugin_cache.insert(name.to_string(), v);
        v
    }

    /// Run `[plugin] fetchers` that are Lua plugins (git status and the
    /// like) for the visible files they have not seen yet.
    fn sync_fetchers(&mut self) {
        if self.cfg.fetchers.is_empty() {
            return;
        }
        let folder = self.tab().current();
        let files: Vec<File> = folder
            .window(self.areas.current.height as usize)
            .filter_map(|i| folder.get(i).cloned())
            .collect();
        let mut batches: Vec<(String, Vec<File>)> = Vec::new();
        for f in files {
            let mime = self.mime(&f).into_owned();
            let names: Vec<String> = self
                .cfg
                .fetchers
                .iter()
                .filter(|r| {
                    r.url.as_ref().is_none_or(|u| u.matches(&f.url, f.is_dir()))
                        && r.mime
                            .as_ref()
                            .is_none_or(|m| !mime.is_empty() && m.matches(&mime))
                })
                .filter_map(|r| r.run.split_whitespace().next().map(String::from))
                .collect();
            for name in names {
                if name.starts_with("mime") {
                    continue;
                }
                let key = (name.clone(), f.url.clone(), f.cha.mtime);
                if self.fetched.contains(&key) || !self.is_plugin(&name) {
                    continue;
                }
                self.fetched.insert(key);
                match batches.iter_mut().find(|b| b.0 == name) {
                    Some(b) => b.1.push(f.clone()),
                    None => batches.push((name, vec![f.clone()])),
                }
            }
        }
        if batches.is_empty() || !self.ensure_lua() {
            return;
        }
        let snap = self.plugin_snapshot_light();
        let mut errs = Vec::new();
        if let Some(rt) = self.lua.as_ref() {
            rt.refresh_cx(&snap);
            for (name, files) in &batches {
                if let Some(e) = rt.fetch(name, files) {
                    errs.push(format!("{name}: {e}"));
                }
            }
            if rt.take_render() {
                self.dirty = true;
            }
        }
        for e in errs {
            self.notify(Level::Error, "Fetcher", &e);
        }
        self.dirty = true;
    }

    /// Whether any Lua UI component (Linemode/Status/Header/Entity) is live.
    pub fn lua_ui_active(&self) -> bool {
        self.lua.as_ref().is_some_and(|rt| {
            [
                "Linemode", "Status", "Header", "Entity", "Current", "Parent", "Modal", "Tab",
                "Root",
            ]
            .iter()
            .any(|c| rt.component_active(c))
        })
    }

    /// Deliver `cd` / `hover` / `tab` events to `ps.sub` subscribers.
    fn sync_ps(&mut self) {
        let Some(rt) = self.lua.as_ref() else { return };
        if rt.is_running() {
            return;
        }
        let cwd = self.cwd();
        let hovered = self.hovered().map(|f| f.url.clone());
        let mut kinds = Vec::new();
        if cwd != self.ps_seen.0 {
            kinds.push("cd");
        }
        if hovered != self.ps_seen.1 {
            kinds.push("hover");
        }
        if self.cur != self.ps_seen.2 {
            kinds.push("tab");
        }
        self.ps_seen = (cwd, hovered, self.cur);
        let kinds: Vec<&str> = kinds.into_iter().filter(|k| rt.has_sub(k)).collect();
        if kinds.is_empty() {
            return;
        }
        let snap = self.plugin_snapshot();
        let mut errs = Vec::new();
        if let Some(rt) = self.lua.as_mut() {
            for k in kinds {
                if let Some(e) = rt.fire(k, &snap) {
                    errs.push(e);
                }
            }
        }
        for e in errs {
            self.notify(Level::Error, "Plugin", &e);
        }
        let (emits, notes) = self.lua.as_ref().map(|rt| rt.drain()).unwrap_or_default();
        for (level, title, body) in notes {
            self.notify(level, &title, &body);
        }
        for c in emits {
            self.run(Layer::Mgr, std::slice::from_ref(&c));
        }
    }
}

pub enum AfterFg {
    Reveal(PathBuf),
    Cd(PathBuf),
    Error(String),
    Info(String),
}

pub fn init_terminal(mouse: bool) {
    use crossterm::QueueableCommand;
    let _ = crossterm::terminal::enable_raw_mode();
    // Queue everything and flush once: one write instead of five.
    let mut out = std::io::stdout();
    let _ = out.queue(crossterm::terminal::EnterAlternateScreen);
    let _ = out.queue(EnableBracketedPaste);
    let _ = out.queue(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    ));
    let _ = out.queue(crossterm::cursor::Hide);
    if mouse {
        let _ = out.queue(EnableMouseCapture);
    }
    let _ = out.flush();
}

pub fn restore_terminal(mouse: bool) {
    let mut out = std::io::stdout();
    if mouse {
        let _ = crossterm::execute!(out, DisableMouseCapture);
    }
    let _ = crossterm::execute!(
        out,
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableFocusChange,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show,
        crossterm::cursor::SetCursorStyle::DefaultUserShape
    );
    let _ = crossterm::terminal::disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Handle messages until nothing arrives for a while (workers finished).
    fn pump(app: &mut App) {
        let mut idle = Instant::now();
        while idle.elapsed() < Duration::from_millis(250) {
            app.sync();
            match app.rx.recv_timeout(Duration::from_millis(20)) {
                Ok(m) => {
                    app.handle(m);
                    idle = Instant::now();
                }
                Err(_) => app.tick(),
            }
        }
        app.sync();
    }

    fn press(app: &mut App, keys: &[&str]) {
        for k in keys {
            app.on_key(Key::parse(k).unwrap());
            app.sync();
        }
        pump(app);
    }

    fn typ(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(Key::plain(crossterm::event::KeyCode::Char(c)));
        }
        app.sync();
    }

    fn screen(app: &mut App) -> String {
        let mut t = Terminal::new(TestBackend::new(app.size.0, app.size.1)).unwrap();
        t.draw(|f| crate::ui::draw(f, app)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    fn names(app: &App) -> Vec<String> {
        app.tab()
            .current()
            .files()
            .map(|f| f.name.to_string())
            .collect()
    }

    #[test]
    fn end_to_end() {
        let _env = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("cfg");
        std::fs::create_dir_all(&cfg).unwrap();
        // SAFETY: this is the only test touching these variables.
        unsafe {
            std::env::set_var("WISP_CONFIG_HOME", &cfg);
            std::env::set_var("YAZI_CONFIG_HOME", &cfg);
            std::env::set_var("XDG_CACHE_HOME", root.path().join("cache"));
            std::env::set_var("XDG_STATE_HOME", root.path().join("state"));
        }
        let work = root.path().join("work");
        std::fs::create_dir_all(work.join("dir")).unwrap();
        std::fs::write(work.join("a.txt"), "alpha needle\n").unwrap();
        std::fs::write(work.join("b.txt"), "beta\n").unwrap();

        let mut app = App::new(Options {
            entry: Some(work.clone()),
            cwd_file: None,
            chooser_file: None,
        });
        app.size = (100, 20);
        app.compute_areas();
        app.load_now(&work);
        app.painted = true;
        pump(&mut app);
        assert_eq!(names(&app), vec!["dir", "a.txt", "b.txt"]);
        let s = screen(&mut app);
        assert!(s.contains("a.txt") && s.contains(" NOR "), "{s}");

        // Yank a.txt, enter dir, paste.
        press(&mut app, &["j", "y", "k", "l"]);
        assert_eq!(app.cwd(), work.join("dir"));
        press(&mut app, &["p"]);
        assert!(work.join("dir/a.txt").exists());
        assert_eq!(names(&app), vec!["a.txt"]);

        // Pasting again makes a renamed copy.
        press(&mut app, &["p"]);
        assert!(work.join("dir/a_1.txt").exists());

        // Back out; the cursor returns to `dir`.
        press(&mut app, &["h"]);
        assert_eq!(app.cwd(), work);
        assert_eq!(&*app.hovered().unwrap().name, "dir");

        // Create a file and a directory.
        press(&mut app, &["a"]);
        typ(&mut app, "new.md");
        press(&mut app, &["<Enter>"]);
        assert!(work.join("new.md").is_file());
        assert_eq!(&*app.hovered().unwrap().name, "new.md");
        press(&mut app, &["a"]);
        typ(&mut app, "sub/deeper/");
        press(&mut app, &["<Enter>"]);
        assert!(work.join("sub/deeper").is_dir());
        // Like yazi, the new entry is revealed: we are now inside `sub/`.
        assert_eq!(app.cwd(), work.join("sub"));
        assert_eq!(&*app.hovered().unwrap().name, "deeper");
        press(&mut app, &["h"]);
        assert_eq!(app.cwd(), work);

        // Rename b.txt → c.txt (cursor starts before the extension).
        press(&mut app, &["G"]);
        let last = app.hovered().unwrap().name.to_string();
        assert_eq!(last, "new.md");
        press(&mut app, &["k"]);
        assert_eq!(&*app.hovered().unwrap().name, "b.txt");
        press(&mut app, &["r", "<BackSpace>"]);
        typ(&mut app, "c");
        press(&mut app, &["<Enter>"]);
        assert!(work.join("c.txt").exists() && !work.join("b.txt").exists());

        // Filter, then clear it with Escape.
        press(&mut app, &["f"]);
        typ(&mut app, "txt");
        press(&mut app, &["<Enter>"]);
        assert_eq!(names(&app), vec!["a.txt", "c.txt"]);
        press(&mut app, &["<Esc>"]);
        assert!(names(&app).len() >= 5);

        // Find jumps to the next match.
        press(&mut app, &["g", "g", "/"]);
        typ(&mut app, "new");
        press(&mut app, &["<Enter>"]);
        assert_eq!(&*app.hovered().unwrap().name, "new.md");

        // Content search finds a.txt (twice: the original and the copy).
        press(&mut app, &["S"]);
        typ(&mut app, "needle");
        press(&mut app, &["<Enter>"]);
        let mut found = names(&app);
        found.sort();
        assert_eq!(found, vec!["a.txt", "a.txt", "a_1.txt"]);
        let s = screen(&mut app);
        assert!(s.contains("(search: needle)"), "{s}");
        assert!(s.contains("dir/"), "search results show their folder: {s}");
        press(&mut app, &["<C-s>"]);
        assert!(app.tab().search.is_none());

        // Selection + trash with confirmation (`d`, then `y`).
        press(&mut app, &["g", "g"]);
        let target = app.hovered().unwrap().url.clone();
        press(&mut app, &["D"]);
        assert!(app.confirm.is_some());
        press(&mut app, &["n"]);
        assert!(target.exists());
        press(&mut app, &["D", "y"]);
        assert!(!target.exists());

        // Tabs.
        press(&mut app, &["t", "t"]);
        assert_eq!(app.tabs.len(), 2);
        assert!(screen(&mut app).contains(" 2 work "));
        press(&mut app, &["1"]);
        assert_eq!(app.cur, 0);
        press(&mut app, &["<C-c>"]);
        assert_eq!(app.tabs.len(), 1);

        // Hidden files toggle and sort/linemode chords.
        std::fs::write(work.join(".secret"), "").unwrap();
        press(&mut app, &["R"]);
        assert!(!names(&app).contains(&".secret".to_string()));
        press(&mut app, &["."]);
        assert!(names(&app).contains(&".secret".to_string()));
        press(&mut app, &[",", "S"]);
        assert_eq!(app.tab().pref.linemode, "size");

        // Visual mode selects a range; Esc keeps it, Esc again clears it.
        press(&mut app, &["g", "g", "v", "j", "j"]);
        assert!(screen(&mut app).contains(" SEL "));
        press(&mut app, &["<Esc>"]);
        assert_eq!(app.tab().selected.len(), 3);
        press(&mut app, &["<Esc>"]);
        assert!(app.tab().selected.is_empty());

        // Which-key popup for chords.
        press(&mut app, &["g"]);
        assert!(screen(&mut app).contains("Go home"));
        press(&mut app, &["<Esc>"]);
        assert!(app.which.is_none());

        // Help lists bindings.
        press(&mut app, &["~"]);
        assert!(screen(&mut app).contains("mgr.help"));
        press(&mut app, &["<Esc>"]);
        assert!(app.help.is_none());

        // Quit writes the cwd file.
        let cwd_file = root.path().join("cwd");
        app.cwd_file = Some(cwd_file.clone());
        press(&mut app, &["q"]);
        assert!(app.quit);
        app.finish();
        assert_eq!(
            std::fs::read_to_string(&cwd_file).unwrap(),
            work.to_string_lossy()
        );
    }
}

/// Split `width` cells by `ratio` exactly as ratatui's solver does for
/// `Constraint::Ratio` columns (rounded cumulative edges), without paying for
/// the solver on start-up.
pub fn split_ratio(x: u16, width: u16, ratio: [u16; 3]) -> [(u16, u16); 3] {
    let all: u32 = ratio.iter().map(|&r| r as u32).sum::<u32>().max(1);
    let mut out = [(x, 0u16); 3];
    let mut cum = 0u32;
    let mut prev = 0u16;
    for i in 0..3 {
        cum += ratio[i] as u32;
        let edge = if i == 2 {
            width
        } else {
            ((width as f64) * (cum as f64) / (all as f64)).round() as u16
        };
        out[i] = (x + prev, edge.saturating_sub(prev));
        prev = edge;
    }
    out
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use ratatui::layout::{Constraint, Direction, Layout};

    #[test]
    fn manual_split_matches_ratatui() {
        let ratios = [
            [1, 4, 3],
            [1, 3, 4],
            [1, 1, 1],
            [0, 1, 1],
            [2, 3, 5],
            [1, 2, 0],
            [0, 1, 0],
            [3, 5, 7],
        ];
        let mut mismatches = Vec::new();
        for r in ratios {
            let all = (r[0] + r[1] + r[2]) as u32;
            for w in 1..400u16 {
                let area = Rect::new(0, 1, w, 10);
                let c = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Ratio(r[0] as u32, all),
                        Constraint::Ratio(r[1] as u32, all),
                        Constraint::Ratio(r[2] as u32, all),
                    ])
                    .split(area);
                let m = split_ratio(0, w, r);
                for i in 0..3 {
                    if (c[i].x, c[i].width) != m[i] {
                        mismatches.push((r, w, i, (c[i].x, c[i].width), m[i]));
                    }
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} mismatches, first: {:?}",
            mismatches.len(),
            &mismatches[..mismatches.len().min(5)]
        );
    }
}

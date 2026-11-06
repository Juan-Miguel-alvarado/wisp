//! Command implementations, one per yazi command name, for every layer.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::app::{
    App, ConfirmKind, ConfirmState, Fg, HelpState, InputKind, InputState, Level, Origin, PickState,
    SpotState, TasksView,
};
use crate::config::{Opener, SortBy};
use crate::folder::{Folder, Pattern};
use crate::fs::File;
use crate::input::{Input, Mode as InputMode};
use crate::keymap::{Cmd, Layer};
use crate::tab::{Mode, Search, Tab};
use crate::tasks::Job;
use crate::util::{
    clean_path, expand_path, full_time, group_name, human_size, on_path, perm_string,
    readable_path, shell_quote, user_name,
};

/// Parse yes/no style option values (`--reverse`, `--reverse=no`).
fn yes_no(cmd: &Cmd, k: &str) -> Option<bool> {
    cmd.opt(k).map(|v| matches!(v, "yes" | "true" | "1"))
}

/// Expand yazi opener placeholders: `%s` all files, `%sN` the Nth, `%d`/`%dN`
/// their directories, `%h` the hovered file, `%%` a literal percent.
pub fn expand_run(run: &str, files: &[PathBuf], hovered: Option<&Path>) -> String {
    let q = |p: &Path| shell_quote(&p.to_string_lossy());
    let dir = |p: &Path| p.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut out = String::with_capacity(run.len() + 64);
    let b = run.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 1 < b.len() {
            let c = b[i + 1];
            let mut j = i + 2;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let n: Option<usize> = run[i + 2..j].parse().ok();
            match c {
                b's' | b'S' => {
                    match n {
                        Some(n) => {
                            if let Some(f) = files.get(n.saturating_sub(1)) {
                                out.push_str(&q(f));
                            }
                        }
                        None => {
                            out.push_str(&files.iter().map(|f| q(f)).collect::<Vec<_>>().join(" "))
                        }
                    }
                    i = j;
                    continue;
                }
                b'd' => {
                    match n {
                        Some(n) => {
                            if let Some(f) = files.get(n.saturating_sub(1)) {
                                out.push_str(&q(&dir(f)));
                            }
                        }
                        None => out.push_str(
                            &files
                                .iter()
                                .map(|f| q(&dir(f)))
                                .collect::<Vec<_>>()
                                .join(" "),
                        ),
                    }
                    i = j;
                    continue;
                }
                b'h' => {
                    if let Some(h) = hovered.or(files.first().map(PathBuf::as_path)) {
                        out.push_str(&q(h));
                    }
                    i += 2;
                    continue;
                }
                b'%' => {
                    out.push('%');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        // Copy one UTF-8 char.
        let ch = run[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Positional args for `sh -c`: `$0` is the hovered file, `$@` the selection.
fn shell_args(files: &[PathBuf], hovered: Option<&Path>) -> Vec<String> {
    let mut v = vec![hovered
        .or(files.first().map(PathBuf::as_path))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()];
    v.extend(files.iter().map(|p| p.to_string_lossy().into_owned()));
    v
}

/// Rename `paths` by editing their names in `$EDITOR`.
pub fn bulk_rename(paths: &[PathBuf]) -> Result<usize, String> {
    let names: Vec<String> = paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let tmp = std::env::temp_dir().join(format!("wisp-bulk-{}.txt", std::process::id()));
    std::fs::write(&tmp, names.join("\n") + "\n").map_err(|e| e.to_string())?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} {}", shell_quote(&tmp.to_string_lossy())))
        .status()
        .map_err(|e| e.to_string())?;
    let edited = std::fs::read_to_string(&tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        return Err("editor exited with an error; nothing renamed".into());
    }
    let new: Vec<&str> = edited.lines().filter(|l| !l.is_empty()).collect();
    if new.len() != names.len() {
        return Err(format!(
            "expected {} lines, got {}; nothing renamed",
            names.len(),
            new.len()
        ));
    }
    let pairs: Vec<(PathBuf, PathBuf)> = paths
        .iter()
        .zip(&new)
        .filter(|(o, n)| o.to_string_lossy() != **n)
        .map(|(o, n)| {
            let n = PathBuf::from(n);
            let n = if n.is_relative() {
                o.parent().unwrap_or(Path::new("/")).join(n)
            } else {
                n
            };
            (o.clone(), n)
        })
        .collect();
    // Two phases through temporary names so swaps (a→b, b→a) work.
    let mut staged = Vec::new();
    for (i, (o, n)) in pairs.iter().enumerate() {
        let t = o.with_file_name(format!(".wisp-bulk-{}-{i}", std::process::id()));
        std::fs::rename(o, &t).map_err(|e| format!("{}: {e}", o.display()))?;
        staged.push((t, n.clone()));
    }
    let mut n = 0;
    for (t, dst) in staged {
        if let Some(p) = dst.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let dst = if dst.exists() {
            crate::util::unique_name(&dst)
        } else {
            dst
        };
        std::fs::rename(&t, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
        n += 1;
    }
    Ok(n)
}

impl App {
    pub fn exec(&mut self, layer: Layer, cmd: &Cmd) {
        let (_, name) = cmd.layer();
        if name == "help" {
            self.input = None;
            self.help = Some(HelpState {
                layer,
                cursor: 0,
                offset: 0,
                filter: String::new(),
            });
            return;
        }
        match layer {
            Layer::Mgr => self.mgr(name, cmd),
            Layer::Tasks => self.tasks_cmd(name, cmd),
            Layer::Spot => self.spot_cmd(name, cmd),
            Layer::Pick => self.pick_cmd(name, cmd),
            Layer::Input => self.input_cmd(cmd),
            Layer::Confirm => self.confirm_cmd(name, cmd),
            Layer::Cmp => self.cmp_cmd(name, cmd),
            Layer::Help => self.help_cmd(name, cmd),
        }
    }

    // ----------------------------------------------------------------------
    // Navigation helpers
    // ----------------------------------------------------------------------

    pub fn cd(&mut self, to: PathBuf) {
        let to = clean_path(&to);
        match std::fs::metadata(&to) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return self.reveal(&to),
            Err(e) => {
                return self.notify(Level::Error, "Cd", &format!("{}: {e}", to.display()));
            }
        }
        if self.tab_mut().cd(to.clone()) {
            self.zoxide_add(&to);
        }
    }

    fn zoxide_add(&self, p: &Path) {
        if !on_path("zoxide") {
            return;
        }
        let p = p.to_path_buf();
        std::thread::spawn(move || {
            let _ = std::process::Command::new("zoxide")
                .arg("add")
                .arg(&p)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        });
    }

    /// Go to `path`'s directory and put the cursor on it.
    pub fn reveal(&mut self, path: &Path) {
        let path = clean_path(path);
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            return self.cd(path);
        };
        if parent != self.cwd() || self.tab().search.is_some() {
            self.cd(parent.clone());
        }
        let found = self
            .tab_mut()
            .folders
            .get_mut(&parent)
            .map(|f| f.hover(&path))
            .unwrap_or(false);
        if !found {
            self.pending_hover.insert(parent, path);
        }
    }

    fn arrow(&mut self, step: &str) {
        let h = self.areas.current.height.max(1) as isize;
        let tab = self.tab_mut();
        let f = tab.current_mut();
        let len = f.len();
        if len == 0 {
            return;
        }
        match step {
            "top" => {
                f.arrow_to(0);
            }
            "bot" => {
                f.arrow_to(len - 1);
            }
            "prev" => {
                if f.cursor == 0 {
                    f.arrow_to(len - 1);
                } else {
                    f.arrow(-1);
                }
            }
            "next" => {
                if f.cursor + 1 >= len {
                    f.arrow_to(0);
                } else {
                    f.arrow(1);
                }
            }
            s => {
                let n: isize = if let Some(p) = s.strip_suffix('%') {
                    let p: isize = p.parse().unwrap_or(0);
                    p * h / 100
                } else {
                    s.parse().unwrap_or(0)
                };
                f.arrow(n);
            }
        }
    }

    fn selected_or_hovered(&self, hovered_only: bool) -> Vec<PathBuf> {
        if hovered_only {
            return self
                .hovered()
                .map(|f| vec![f.url.clone()])
                .unwrap_or_default();
        }
        self.tab().selected_or_hovered()
    }

    fn open_input(&mut self, kind: InputKind, title: String, value: &str, origin: Origin) {
        let mut input = Input::new(value, InputMode::Insert);
        input.set_cursor(value.chars().count());
        self.input = Some(InputState {
            kind,
            input,
            title,
            origin,
            cmp: Vec::new(),
            cmp_cursor: 0,
            cmp_shown: false,
        });
        self.on_input_change();
    }

    // ----------------------------------------------------------------------
    // mgr layer
    // ----------------------------------------------------------------------

    fn mgr(&mut self, name: &str, cmd: &Cmd) {
        match name {
            "escape" => self.escape(cmd),
            "quit" => {
                let no_cwd = cmd.flag("no-cwd-file");
                if self.running_tasks() > 0 && !cmd.flag("force") {
                    self.confirm = Some(ConfirmState {
                        kind: ConfirmKind::Quit { no_cwd },
                        title: self.cfg.title("confirm", "quit_title", None),
                        body: self.cfg.title("confirm", "quit_body", None),
                        list: Vec::new(),
                        offset: 0,
                    });
                } else {
                    self.write_cwd = !no_cwd;
                    self.quit = true;
                }
            }
            "close" => {
                if self.tabs.len() > 1 {
                    let i = self.cur;
                    self.close_tab(i);
                } else {
                    self.mgr("quit", cmd);
                }
            }
            "suspend" => self.fg = Some(Fg::Suspend),
            "arrow" => self.arrow(cmd.arg(0).unwrap_or("0")),
            "leave" => {
                if self.tab().search.is_some() {
                    self.tab_mut().end_search();
                    return;
                }
                let cwd = self.cwd();
                if let Some(p) = cwd.parent() {
                    let p = p.to_path_buf();
                    self.cd(p.clone());
                    if let Some(f) = self.tab_mut().folders.get_mut(&p) {
                        f.hover(&cwd);
                    }
                }
            }
            "enter" => {
                if let Some(d) = self.tab().hovered_dir() {
                    self.cd(d);
                }
            }
            "back" => {
                self.tab_mut().back();
            }
            "forward" => {
                self.tab_mut().forward();
            }
            "toggle" => {
                let url = match cmd.arg(0) {
                    Some(u) => Some(PathBuf::from(u)),
                    None => self.hovered().map(|f| f.url.clone()),
                };
                if let Some(u) = url {
                    let tab = self.tab_mut();
                    let on = match cmd.opt("state") {
                        Some("on") => true,
                        Some("off") => false,
                        _ => !tab.selected.contains(&u),
                    };
                    if on {
                        tab.selected.insert(u);
                    } else {
                        tab.selected.remove(&u);
                    }
                }
            }
            "toggle_all" | "select_all" => {
                let state = cmd
                    .opt("state")
                    .map(String::from)
                    .or_else(|| (name == "select_all").then(|| "on".to_string()));
                let tab = self.tab_mut();
                let urls: Vec<PathBuf> = tab.current().files().map(|f| f.url.clone()).collect();
                for u in urls {
                    let on = match state.as_deref() {
                        Some("on" | "true") => true,
                        Some("off" | "false") => false,
                        _ => !tab.selected.contains(&u),
                    };
                    if on {
                        tab.selected.insert(u);
                    } else {
                        tab.selected.remove(&u);
                    }
                }
            }
            "visual_mode" => {
                let tab = self.tab_mut();
                if tab.mode != Mode::Normal {
                    tab.commit_visual();
                    return;
                }
                let c = tab.current().cursor;
                tab.mode = if cmd.flag("unset") {
                    Mode::Unset(c)
                } else {
                    Mode::Select(c)
                };
            }
            "seek" => {
                let units: isize = cmd.arg(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                self.seek(units);
            }
            "spot" => self.spot_open(),
            "open" => self.open(cmd),
            "yank" => {
                let paths = self.selected_or_hovered(false);
                if paths.is_empty() {
                    return;
                }
                self.yanked.cut = cmd.flag("cut");
                self.yanked.paths = paths.into_iter().collect();
                self.share_yank();
                let tab = self.tab_mut();
                tab.selected.clear();
                tab.mode = Mode::Normal;
            }
            "unyank" => {
                self.yanked.paths.clear();
                self.share_yank();
            }
            "paste" => {
                self.adopt_shared_yank();
                if self.yanked.paths.is_empty() {
                    return;
                }
                let srcs: Vec<PathBuf> = self.yanked.paths.iter().cloned().collect();
                let dst_dir = self.cwd();
                let force = cmd.flag("force");
                if self.yanked.cut {
                    self.spawn_task(Job::Move {
                        srcs,
                        dst_dir,
                        force,
                    });
                    self.yanked.paths.clear();
                    self.share_yank();
                } else {
                    self.spawn_task(Job::Copy {
                        srcs,
                        dst_dir,
                        force,
                        follow: cmd.flag("follow"),
                    });
                }
            }
            "link" => {
                self.adopt_shared_yank();
                if self.yanked.paths.is_empty() || self.yanked.cut {
                    return;
                }
                let srcs: Vec<PathBuf> = self.yanked.paths.iter().cloned().collect();
                let dst_dir = self.cwd();
                self.spawn_task(Job::Link {
                    srcs,
                    dst_dir,
                    relative: cmd.flag("relative"),
                    force: cmd.flag("force"),
                });
            }
            "hardlink" => {
                self.adopt_shared_yank();
                if self.yanked.paths.is_empty() || self.yanked.cut {
                    return;
                }
                let srcs: Vec<PathBuf> = self.yanked.paths.iter().cloned().collect();
                let dst_dir = self.cwd();
                self.spawn_task(Job::Hardlink {
                    srcs,
                    dst_dir,
                    force: cmd.flag("force"),
                });
            }
            "remove" => {
                let paths = self.selected_or_hovered(cmd.flag("hovered"));
                if paths.is_empty() {
                    return;
                }
                let permanently = cmd.flag("permanently");
                if cmd.flag("force") {
                    self.remove_do(paths, permanently);
                    return;
                }
                let n = paths.len();
                let s = if n == 1 { "" } else { "s" };
                let key = if permanently {
                    "delete_title"
                } else {
                    "trash_title"
                };
                let title = self
                    .cfg
                    .title("confirm", key, None)
                    .replace("{n}", &n.to_string())
                    .replace("{s}", s);
                let list = paths
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                self.confirm = Some(ConfirmState {
                    kind: if permanently {
                        ConfirmKind::Delete(paths)
                    } else {
                        ConfirmKind::Trash(paths)
                    },
                    title,
                    body: String::new(),
                    list,
                    offset: 0,
                });
            }
            "create" => {
                let dir = cmd.flag("dir");
                let title = self.cfg.title("input", "create_title", Some(dir as usize));
                self.open_input(
                    InputKind::Create {
                        dir,
                        force: cmd.flag("force"),
                    },
                    title,
                    "",
                    Origin::TopCenter,
                );
            }
            "rename" => self.rename(cmd),
            "shell" => {
                let run = cmd.arg(0).unwrap_or("").to_string();
                let block = cmd.flag("block");
                let orphan = cmd.flag("orphan");
                if cmd.flag("interactive") || run.is_empty() {
                    let title = self.cfg.title("input", "shell_title", Some(block as usize));
                    self.open_input(
                        InputKind::Shell { block, orphan },
                        title,
                        &run,
                        Origin::TopCenter,
                    );
                    if let (Some(c), Some(i)) = (
                        cmd.opt("cursor").and_then(|c| c.parse::<usize>().ok()),
                        self.input.as_mut(),
                    ) {
                        i.input.set_cursor(c);
                    }
                } else {
                    self.shell_do(&run, block, orphan);
                }
            }
            "hidden" => {
                let tab = self.tab_mut();
                tab.pref.show_hidden = match cmd.arg(0) {
                    Some("show") => true,
                    Some("hide") => false,
                    _ => !tab.pref.show_hidden,
                };
                self.apply_prefs();
            }
            "search" | "search_do" => {
                let via = cmd
                    .opt("via")
                    .or(cmd
                        .arg(0)
                        .filter(|a| matches!(*a, "fd" | "rg" | "rga" | "deep" | "none")))
                    .unwrap_or("fd")
                    .to_string();
                if via == "none" {
                    self.tab_mut().end_search();
                    return;
                }
                if let Some(q) = cmd.opt("query").or(cmd.arg(1)) {
                    let q = q.to_string();
                    self.search_do(&via, &q);
                    return;
                }
                let title = self
                    .cfg
                    .title("input", "search_title", None)
                    .replace("{n}", &via);
                self.open_input(InputKind::Search { via }, title, "", Origin::TopCenter);
            }
            "plugin" => self.plugin(cmd),
            "linemode" => {
                if let Some(m) = cmd.arg(0) {
                    self.tab_mut().pref.linemode = m.to_string();
                }
            }
            "copy" => self.copy(cmd),
            "filter" => {
                let prev = self
                    .tab()
                    .current()
                    .filter
                    .as_ref()
                    .map(|p| p.query.clone());
                let title = self.cfg.title("input", "filter_title", None);
                self.open_input(
                    InputKind::Filter {
                        smart: cmd.flag("smart"),
                        insensitive: cmd.flag("insensitive"),
                        prev: prev.clone(),
                    },
                    title,
                    prev.as_deref().unwrap_or(""),
                    Origin::TopCenter,
                );
            }
            "find" => {
                let prev = cmd.flag("previous");
                let title = self.cfg.title("input", "find_title", Some(prev as usize));
                self.open_input(
                    InputKind::Find {
                        prev,
                        smart: cmd.flag("smart"),
                        insensitive: cmd.flag("insensitive"),
                    },
                    title,
                    "",
                    Origin::TopCenter,
                );
            }
            "find_arrow" => self.find_arrow(cmd.flag("previous"), false),
            "sort" => {
                let tab = self.tab_mut();
                let mut s = tab.pref.sort;
                if let Some(by) = cmd.arg(0).and_then(SortBy::parse) {
                    s.by = by;
                }
                if let Some(r) = yes_no(cmd, "reverse") {
                    s.reverse = r;
                }
                if let Some(r) = yes_no(cmd, "dir-first") {
                    s.dir_first = r;
                }
                if let Some(r) = yes_no(cmd, "sensitive") {
                    s.sensitive = r;
                }
                tab.pref.sort = s;
                self.apply_prefs();
            }
            "cd" => {
                if cmd.flag("interactive") {
                    let mut start = readable_path(&self.cwd());
                    if !start.ends_with('/') {
                        start.push('/');
                    }
                    let title = self.cfg.title("input", "cd_title", None);
                    self.open_input(InputKind::Cd, title, &start, Origin::TopCenter);
                } else if let Some(p) = cmd.arg(0) {
                    let p = self.cwd().join(expand_path(p));
                    self.cd(p);
                }
            }
            "reveal" => {
                if let Some(p) = cmd.arg(0) {
                    let p = self.cwd().join(expand_path(p));
                    self.reveal(&p);
                }
            }
            "follow" => {
                let target = self.hovered().and_then(|f| {
                    let t = f.link_to.clone()?;
                    Some(if t.is_relative() {
                        f.url.parent().unwrap_or(Path::new("/")).join(t)
                    } else {
                        t
                    })
                });
                if let Some(t) = target {
                    self.reveal(&t);
                }
            }
            "tab_create" => {
                if self.tabs.len() >= 9 {
                    return self.notify(Level::Warn, "Tabs", "Too many tabs (max 9)");
                }
                let path = if let Some(p) = cmd.arg(0) {
                    self.cwd().join(expand_path(p))
                } else if cmd.flag("current") {
                    self.cwd()
                } else {
                    crate::util::home().clone().unwrap_or_else(|| self.cwd())
                };
                let pref = self.tab().pref.clone();
                let mut tab = Tab::new(path, pref);
                // Start the new tab where the old one was looking.
                if let Some(h) = self.hovered().map(|f| f.url.clone()) {
                    if let Some(f) = self.tab().folders.get(&tab.cwd).cloned() {
                        tab.folders.insert(tab.cwd.clone(), f);
                        if let Some(f) = tab.folders.get_mut(&tab.cwd.clone()) {
                            f.hover(&h);
                        }
                    }
                }
                self.tabs.insert(self.cur + 1, tab);
                self.cur += 1;
            }
            "tab_rename" => {
                if cmd.flag("interactive") || cmd.arg(0).is_none() {
                    let cur = self.tab().title();
                    self.open_input(
                        InputKind::TabRename,
                        "Rename tab:".into(),
                        &cur,
                        Origin::TopCenter,
                    );
                } else {
                    let n = cmd.arg(0).unwrap_or("").to_string();
                    self.tab_mut().name = (!n.is_empty()).then_some(n);
                }
            }
            "tab_close" => {
                let i: usize = cmd.arg(0).and_then(|s| s.parse().ok()).unwrap_or(self.cur);
                self.close_tab(i);
            }
            "tab_switch" => {
                let n: isize = cmd.arg(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                let len = self.tabs.len() as isize;
                let i = if cmd.flag("relative") {
                    (self.cur as isize + n).rem_euclid(len)
                } else {
                    n
                };
                if i >= 0 && i < len {
                    self.cur = i as usize;
                }
            }
            "tab_swap" => {
                let n: isize = cmd.arg(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                let len = self.tabs.len() as isize;
                let j = (self.cur as isize + n).rem_euclid(len) as usize;
                self.tabs.swap(self.cur, j);
                self.cur = j;
            }
            "tasks_show" | "show" => {
                self.tasks_view = Some(TasksView { cursor: 0 });
            }
            "refresh" => {
                self.mimes.clear();
                let cwd = self.cwd();
                self.refresh_dir(&cwd);
                if let Some(p) = self.tab().parent_path() {
                    self.refresh_dir(&p);
                }
                if let Some(h) = self.hovered().map(|f| f.url.clone()) {
                    self.previewer.invalidate(&h);
                    self.preview_key = None;
                    if h.is_dir() {
                        self.refresh_dir(&h);
                    }
                }
            }
            "calc_size" => {
                let paths: Vec<PathBuf> = self
                    .selected_or_hovered(false)
                    .into_iter()
                    .filter(|p| p.is_dir())
                    .collect();
                for p in paths {
                    self.calc_size(p);
                }
            }
            "noop" | "resize" => {}
            // Move in visual mode, re-anchoring at the old cursor (swap ends).
            "visual_arrow" => {
                let n: isize = cmd.arg(0).and_then(|s| s.parse().ok()).unwrap_or(0);
                let tab = self.tab_mut();
                let cur = tab.current().cursor;
                tab.mode = match tab.mode {
                    Mode::Select(_) => Mode::Select(cur),
                    Mode::Unset(_) => Mode::Unset(cur),
                    Mode::Normal => Mode::Normal,
                };
                tab.current_mut().arrow(n);
            }
            // What plugins emit to drive filter/find directly.
            "filter_do" => {
                let q = cmd.arg(0).unwrap_or("");
                let p = Pattern::new(q, cmd.flag("smart"), cmd.flag("insensitive"));
                self.tab_mut().current_mut().set_filter(p);
            }
            "find_do" => {
                let q = cmd.arg(0).unwrap_or("");
                self.tab_mut().finder = Pattern::new(q, cmd.flag("smart"), cmd.flag("insensitive"));
                self.find_arrow(cmd.flag("previous"), true);
            }
            "update_files" => {
                let d = cmd.arg(0).map(PathBuf::from).unwrap_or_else(|| self.cwd());
                self.refresh_dir(&d);
            }
            "__show_image" => {
                if let Some(p) = cmd.arg(0) {
                    let p = PathBuf::from(p);
                    self.show_image(&p);
                }
            }
            "__search_begin" => {
                let domain = cmd.arg(0).unwrap_or("search").to_string();
                let id = self.next_search;
                self.next_search += 1;
                let cwd = self.cwd();
                let tab = self.tab_mut();
                tab.end_search();
                tab.finder = None;
                tab.search = Some(Search {
                    id,
                    query: domain,
                    folder: Folder::new(cwd),
                    cancel: Arc::new(AtomicBool::new(false)),
                    done: false,
                });
            }
            "__search_files" => {
                let files: Vec<File> = cmd
                    .args
                    .iter()
                    .filter_map(|p| File::from_path(Path::new(p)).ok())
                    .collect();
                let tab = self.tab_mut();
                let sort = tab.pref.sort;
                if let Some(s) = tab.search.as_mut() {
                    let mut items = std::mem::take(&mut s.folder.items);
                    items.extend(files);
                    s.folder.set_items(items, sort, true, false);
                }
            }
            "__search_done" => {
                let tab = self.tab_mut();
                let sort = tab.pref.sort;
                if let Some(s) = tab.search.as_mut() {
                    s.done = true;
                    if s.folder.stage == crate::folder::Stage::Loading {
                        s.folder.set_items(Vec::new(), sort, true, true);
                    }
                }
            }
            other => {
                self.notify(Level::Warn, "Keymap", &format!("Unknown command `{other}`"));
            }
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i >= self.tabs.len() {
            return;
        }
        if self.tabs.len() == 1 {
            self.quit = true;
            return;
        }
        self.tabs[i].end_search();
        self.tabs.remove(i);
        if self.cur >= self.tabs.len() || self.cur > i {
            self.cur = self.cur.saturating_sub(1);
        }
    }

    fn apply_prefs(&mut self) {
        let tab = self.tab_mut();
        let (sort, hidden) = (tab.pref.sort, tab.pref.show_hidden);
        for f in tab.folders.values_mut() {
            f.apply_prefs(sort, hidden);
        }
        if let Some(s) = &mut tab.search {
            s.folder.apply_prefs(sort, true);
        }
    }

    fn escape(&mut self, cmd: &Cmd) {
        let all = cmd.flag("all");
        let any_flag = ["find", "visual", "filter", "select", "search"]
            .iter()
            .any(|f| cmd.flag(f));
        let want = |f: &str| all || cmd.flag(f);
        let tab = self.tab_mut();
        let find = |tab: &mut Tab| tab.finder.take().is_some();
        let visual = |tab: &mut Tab| {
            if tab.mode != Mode::Normal {
                tab.commit_visual();
                true
            } else {
                false
            }
        };
        let filter = |tab: &mut Tab| {
            if tab.current().filter.is_some() {
                tab.current_mut().set_filter(None);
                true
            } else {
                false
            }
        };
        let select = |tab: &mut Tab| {
            if tab.selected.is_empty() {
                false
            } else {
                tab.selected.clear();
                true
            }
        };
        let search = |tab: &mut Tab| {
            if tab.search.is_some() {
                tab.end_search();
                true
            } else {
                false
            }
        };
        if !any_flag && !all {
            let _ = find(tab) || visual(tab) || filter(tab) || select(tab) || search(tab);
            return;
        }
        if want("find") {
            find(tab);
        }
        if want("visual") {
            visual(tab);
        }
        if want("filter") {
            filter(tab);
        }
        if want("select") {
            select(tab);
        }
        if want("search") {
            search(tab);
        }
    }

    fn seek(&mut self, units: isize) {
        let Some(f) = self.hovered().cloned() else {
            return;
        };
        if f.is_dir() {
            if let Some(folder) = self.tab_mut().folders.get_mut(&f.url) {
                folder.arrow(units);
            }
            return;
        }
        let mime = self.mime(&f).into_owned();
        let h = self.areas.preview.height.max(1) as isize;
        let step = if mime == "application/pdf" || mime.starts_with("video/") {
            units.signum()
        } else {
            let s = units * h / 10;
            if s == 0 {
                units.signum()
            } else {
                s
            }
        };
        self.preview_skip = (self.preview_skip as isize + step).max(0) as usize;
    }

    fn open(&mut self, cmd: &Cmd) {
        let targets = if cmd.args.is_empty() {
            self.selected_or_hovered(cmd.flag("hovered"))
        } else {
            cmd.args
                .iter()
                .map(|a| self.cwd().join(expand_path(a)))
                .collect()
        };
        if targets.is_empty() {
            return;
        }
        let interactive = cmd.flag("interactive");
        if let Some(_f) = &self.chooser_file {
            // File-chooser mode: hand the selection back to the caller.
            let only_dir = targets.len() == 1 && targets[0].is_dir();
            if !(only_dir && !interactive && self.tab().selected.is_empty()) {
                self.chosen = Some(targets);
                self.write_cwd = false;
                self.quit = true;
                return;
            }
        }
        // Opening a single hovered directory enters it.
        if !interactive && self.tab().selected.is_empty() && targets.len() == 1 {
            if let Some(h) = self.hovered() {
                if h.is_dir() && h.url == targets[0] {
                    let d = h.url.clone();
                    return self.cd(d);
                }
            }
        }
        let first = &targets[0];
        let file = self
            .tab()
            .current()
            .files()
            .find(|f| &f.url == first)
            .cloned()
            .or_else(|| File::from_path(first).ok());
        let Some(file) = file else { return };
        let mime = {
            let m = self.mime(&file).into_owned();
            if m.is_empty() {
                crate::fs::mime_of(&file)
            } else {
                m
            }
        };
        let openers = self.cfg.openers_for(&file.url, file.is_dir(), &mime);
        if openers.is_empty() {
            return self.notify(Level::Warn, "Open", &format!("No opener for {mime}"));
        }
        if interactive {
            self.pick = Some(PickState {
                title: self.cfg.title("pick", "open_title", None),
                items: openers,
                cursor: 0,
                targets,
            });
            return;
        }
        self.run_opener(&openers[0], &targets);
    }

    /// Publish the yank list so other running instances can paste it
    /// (yazi shares yanks across instances too).
    fn share_yank(&mut self) {
        let Ok(dir) = crate::config::state_dir() else {
            return;
        };
        let mut s = String::from(if self.yanked.cut { "cut\n" } else { "copy\n" });
        for p in &self.yanked.paths {
            s.push_str(&p.to_string_lossy());
            s.push('\n');
        }
        let f = dir.join("yank");
        let _ = std::fs::write(&f, s);
        self.yank_stamp = std::fs::metadata(&f).and_then(|m| m.modified()).ok();
    }

    /// Take over a yank made in another instance, if it is newer than ours.
    fn adopt_shared_yank(&mut self) {
        let Ok(dir) = crate::config::state_dir() else {
            return;
        };
        let f = dir.join("yank");
        let Ok(stamp) = std::fs::metadata(&f).and_then(|m| m.modified()) else {
            return;
        };
        if self.yank_stamp.is_some_and(|s| s >= stamp) {
            return;
        }
        self.yank_stamp = Some(stamp);
        let Ok(text) = std::fs::read_to_string(&f) else {
            return;
        };
        let mut lines = text.lines();
        let cut = lines.next() == Some("cut");
        let paths: std::collections::BTreeSet<PathBuf> = lines
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect();
        self.yanked.cut = cut;
        self.yanked.paths = paths;
    }

    pub fn run_opener(&mut self, o: &Opener, files: &[PathBuf]) {
        // yazi's preset hands archives to `ya pub extract`; do it natively so
        // no yazi install is needed.
        if o.run.trim_start().starts_with("ya pub extract") {
            for f in files {
                self.spawn_task(Job::Extract {
                    archive: f.clone(),
                    dst_dir: self.cwd(),
                });
            }
            return;
        }
        let hovered = self.hovered().map(|f| f.url.clone());
        let script = expand_run(&o.run, files, hovered.as_deref());
        let args = shell_args(files, hovered.as_deref());
        let cwd = self.cwd();
        if o.block {
            self.fg = Some(Fg::Shell {
                script,
                args,
                cwd,
                wait_key: false,
            });
        } else {
            let mut a = vec!["-c".to_string(), script];
            a.extend(args);
            self.spawn_task(Job::Process {
                title: format!("Run {}", o.label()),
                program: "sh".into(),
                args: a,
                cwd,
                orphan: o.orphan,
            });
        }
    }

    fn shell_do(&mut self, run: &str, block: bool, orphan: bool) {
        let files = self.tab().selected_or_hovered();
        let hovered = self.hovered().map(|f| f.url.clone());
        let script = expand_run(run, &files, hovered.as_deref());
        let args = shell_args(&files, hovered.as_deref());
        let cwd = self.cwd();
        if block {
            self.fg = Some(Fg::Shell {
                script,
                args,
                cwd,
                wait_key: false,
            });
        } else {
            let mut a = vec!["-c".to_string(), script];
            a.extend(args);
            self.spawn_task(Job::Process {
                title: format!("Background command: {run}"),
                program: "sh".into(),
                args: a,
                cwd,
                orphan,
            });
        }
    }

    fn remove_do(&mut self, paths: Vec<PathBuf>, permanently: bool) {
        for p in &paths {
            self.yanked.paths.remove(p);
            self.tab_mut().selected.remove(p);
        }
        if permanently {
            self.spawn_task(Job::Delete { paths });
        } else {
            self.spawn_task(Job::Trash { paths });
        }
    }

    fn rename(&mut self, cmd: &Cmd) {
        let tab = self.tab();
        if !cmd.flag("hovered") && tab.selected.len() > 1 {
            let paths: Vec<PathBuf> = tab.selected.iter().cloned().collect();
            self.fg = Some(Fg::BulkRename(paths));
            return;
        }
        let Some(f) = self.hovered().cloned() else {
            return;
        };
        let name = f.name.to_string();
        let ext_len = if f.is_dir() {
            None
        } else {
            f.ext().map(|e| e.chars().count())
        };
        let total = name.chars().count();
        let value = match cmd.opt("empty") {
            Some("all") => String::new(),
            Some("stem") => match ext_len {
                Some(e) => name.chars().skip(total - e - 1).collect(),
                None => String::new(),
            },
            Some("ext") => match ext_len {
                Some(e) => name.chars().take(total - e).collect(),
                None => name.clone(),
            },
            Some("dot_ext") => match ext_len {
                Some(e) => name.chars().take(total - e - 1).collect(),
                None => name.clone(),
            },
            _ => name.clone(),
        };
        let vlen = value.chars().count();
        let cursor = match cmd.opt("cursor") {
            Some("start") => 0,
            Some("before_ext") => match (ext_len, cmd.opt("empty")) {
                (Some(e), None) => vlen - e - 1,
                (Some(_), Some("stem")) => 0,
                _ => vlen,
            },
            _ => vlen,
        };
        let title = self.cfg.title("input", "rename_title", None);
        self.open_input(
            InputKind::Rename {
                from: f.url.clone(),
                force: cmd.flag("force"),
            },
            title,
            &value,
            Origin::Hovered,
        );
        if let Some(i) = self.input.as_mut() {
            i.input.set_cursor(cursor);
        }
    }

    fn rename_do(&mut self, from: PathBuf, to_name: &str, force: bool) {
        if to_name.is_empty() {
            return;
        }
        let to = from
            .parent()
            .unwrap_or(Path::new("/"))
            .join(expand_path(to_name));
        if to == from {
            return;
        }
        if !force && std::fs::symlink_metadata(&to).is_ok() {
            let body = self.cfg.title("confirm", "overwrite_body", None);
            self.confirm = Some(ConfirmState {
                kind: ConfirmKind::RenameOverwrite {
                    from,
                    to: to.clone(),
                },
                title: self.cfg.title("confirm", "overwrite_title", None),
                body,
                list: vec![readable_path(&to)],
                offset: 0,
            });
            return;
        }
        if let Some(p) = to.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => {
                self.previewer.invalidate(&from);
                let tab = self.tab_mut();
                if tab.selected.remove(&from) {
                    tab.selected.insert(to.clone());
                }
                if let Some(f) = tab
                    .folders
                    .get_mut(&from.parent().unwrap_or(Path::new("/")).to_path_buf())
                {
                    f.remove(&from);
                    if to.parent() == from.parent() {
                        if let Ok(nf) = File::from_path(&to) {
                            f.upsert(nf);
                            f.hover(&to);
                        }
                    }
                }
                if to.parent() != from.parent() {
                    self.reveal(&to);
                }
                if let Some(p) = from.parent() {
                    self.refresh_dir(p);
                }
            }
            Err(e) => self.notify(Level::Error, "Rename", &e.to_string()),
        }
    }

    fn create_do(&mut self, name: &str, dir: bool, force: bool) {
        if name.is_empty() {
            return;
        }
        let is_dir = dir || name.ends_with('/');
        let path = clean_path(&self.cwd().join(expand_path(name)));
        if !force && std::fs::symlink_metadata(&path).is_ok() {
            self.confirm = Some(ConfirmState {
                kind: ConfirmKind::CreateOverwrite {
                    path: path.clone(),
                    dir: is_dir,
                },
                title: self.cfg.title("confirm", "overwrite_title", None),
                body: self.cfg.title("confirm", "overwrite_body", None),
                list: vec![readable_path(&path)],
                offset: 0,
            });
            return;
        }
        let r = if is_dir {
            std::fs::create_dir_all(&path)
        } else {
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .map(|_| ())
        };
        match r {
            Ok(()) => {
                let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
                if parent == self.cwd() {
                    if let Ok(nf) = File::from_path(&path) {
                        if let Some(f) = self.tab_mut().folders.get_mut(&parent) {
                            f.upsert(nf);
                        }
                    }
                }
                self.reveal(&path);
                self.refresh_dir(&parent);
            }
            Err(e) => self.notify(Level::Error, "Create", &e.to_string()),
        }
    }

    fn search_do(&mut self, via: &str, query: &str) {
        if query.is_empty() {
            return;
        }
        let id = self.next_search;
        self.next_search += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let cwd = self.cwd();
        let hidden = self.tab().pref.show_hidden;
        let tab = self.tab_mut();
        tab.end_search();
        tab.finder = None;
        tab.search = Some(Search {
            id,
            query: query.to_string(),
            folder: Folder::new(cwd.clone()),
            cancel: cancel.clone(),
            done: false,
        });
        crate::search::spawn(
            crate::search::Opts {
                id,
                via: via.to_string(),
                root: cwd,
                query: query.to_string(),
                hidden,
                cancel,
            },
            self.tx.clone(),
        );
    }

    fn plugin(&mut self, cmd: &Cmd) {
        let name = cmd.arg(0).unwrap_or("");
        let arg = cmd.arg(1).or(cmd.opt("args")).unwrap_or("");
        // An installed Lua plugin wins over the built-in equivalents.
        if !name.is_empty() && crate::plugin::find_plugin(name).is_some() {
            let name = name.to_string();
            return self.plugin_start(&name, cmd);
        }
        match name {
            "fzf" => {
                if on_path("fzf") {
                    self.fg = Some(Fg::Fzf);
                } else {
                    self.notify(Level::Warn, "fzf", "fzf is not installed");
                }
            }
            "zoxide" => {
                if on_path("zoxide") {
                    self.fg = Some(Fg::Zoxide);
                } else {
                    self.notify(Level::Warn, "zoxide", "zoxide is not installed");
                }
            }
            "smart-enter" => {
                if self.tab().hovered_dir().is_some() {
                    self.mgr("enter", cmd);
                } else {
                    self.open(&Cmd::new("open"));
                }
            }
            "toggle-pane" | "max-preview" | "hide-preview" => {
                let a = if name == "toggle-pane" { arg } else if name == "max-preview" { "max-preview" } else { "min-preview" };
                let (min, which) = match a.split_once('-') {
                    Some(("min", w)) => (true, w),
                    Some(("max", w)) => (false, w),
                    _ => (false, "reset"),
                };
                let i = match which {
                    "parent" => 0,
                    "current" => 1,
                    "preview" => 2,
                    _ => {
                        self.pane = [true, true, true];
                        return;
                    }
                };
                if min {
                    self.pane[i] = !self.pane[i];
                } else {
                    let only = self.pane.iter().enumerate().all(|(j, v)| *v == (j == i));
                    self.pane = if only { [true, true, true] } else { [i == 0, i == 1, i == 2] };
                    if !self.pane[1] {
                        self.pane[1] = true; // never hide the list entirely
                        if i == 2 {
                            self.pane = [false, true, true];
                        }
                    }
                }
                self.preview_key = None;
            }
            "" => {}
            other => self.notify(
                Level::Warn,
                "Plugin",
                &format!("Plugin `{other}` is not available (Lua plugins are not supported; bind a `shell` command instead)"),
            ),
        }
    }

    fn copy(&mut self, cmd: &Cmd) {
        let what = cmd.arg(0).unwrap_or("path");
        let paths = self.selected_or_hovered(cmd.flag("hovered"));
        let parts: Vec<String> = paths
            .iter()
            .map(|p| match what {
                "dirname" => p
                    .parent()
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "filename" => p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                "name_without_ext" => p
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                _ => p.to_string_lossy().into_owned(),
            })
            .collect();
        let text = parts.join("\n");
        if let Err(e) = crate::clip::copy(&text) {
            self.notify(Level::Error, "Copy", &e);
        }
    }

    fn find_arrow(&mut self, prev: bool, include_current: bool) {
        let tab = self.tab_mut();
        let Some(p) = tab.finder.clone() else { return };
        let f = tab.current_mut();
        let len = f.len();
        if len == 0 {
            return;
        }
        let start = f.cursor;
        for k in 0..len {
            let step = if include_current { k } else { k + 1 };
            let i = if prev {
                (start + len * 2 - step) % len
            } else {
                (start + step) % len
            };
            if f.get(i).is_some_and(|x| p.is_match(&x.name)) {
                f.arrow_to(i);
                return;
            }
        }
    }

    fn calc_size(&mut self, p: PathBuf) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let (size, items) = dir_size(&p);
            let _ = tx.send(crate::event::Msg::DirSize {
                path: p,
                size,
                items,
            });
        });
    }

    fn spot_open(&mut self) {
        let Some(f) = self.hovered().cloned() else {
            return;
        };
        let mut rows = self.spot_rows(&f);
        // A `[plugin] spotters` rule naming a Lua plugin replaces the table.
        let mime = {
            let m = self.mime(&f).into_owned();
            if m.is_empty() {
                crate::fs::mime_of(&f)
            } else {
                m
            }
        };
        let spotter = self
            .cfg
            .spotter_for(&f.url, f.is_dir(), &mime)
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        if !spotter.is_empty() && self.is_plugin(&spotter) && self.ensure_lua() {
            let snap = self.plugin_snapshot();
            let r = self.lua.as_ref().map(|rt| {
                rt.refresh_cx(&snap);
                rt.spot(&spotter, &f, &mime)
            });
            match r {
                Some(Ok(Some(custom))) => rows = custom,
                Some(Err(e)) => self.notify(Level::Error, "Spotter", &e),
                _ => {}
            }
        }
        if f.is_dir() && !self.dir_sizes.contains_key(&f.url) {
            self.calc_size(f.url.clone());
        }
        // yazi starts on the first value row (`:row(1)`).
        let cursor = rows.iter().position(|r| !r.1.is_empty()).unwrap_or(0);
        self.spot = Some(SpotState {
            url: f.url.clone(),
            rows,
            cursor,
        });
    }

    /// Spot rows in yazi's layout: a kind-specific section (Folder / Image /
    /// Video), then `Base` and `Plugins`, and finally a `Details` section
    /// yazi does not have. Section headers have no value and no indent.
    fn spot_rows(&self, f: &File) -> Vec<(String, String)> {
        let mime = {
            let m = self.mime(f).into_owned();
            if m.is_empty() {
                crate::fs::mime_of(f)
            } else {
                m
            }
        };
        let c = &f.cha;
        let h = |s: &str| (s.to_string(), String::new());
        let r = |k: &str, v: String| (format!("  {k}:"), v);
        let blank = || (String::new(), String::new());
        let mut rows = Vec::new();

        if f.is_dir() {
            rows.push(h("Folder"));
            let size = match self.dir_sizes.get(&f.url) {
                Some((s, _)) => crate::util::readable_size(*s),
                None => "0B (?)".into(),
            };
            rows.push(r("Size", size));
            rows.push(blank());
        } else if mime.starts_with("image/") {
            if let Ok(reader) =
                image::ImageReader::open(&f.url).and_then(|r| r.with_guessed_format())
            {
                let format = reader
                    .format()
                    .map(|f| format!("{f:?}"))
                    .unwrap_or_else(|| "?".into());
                if let Ok((w, hh)) = reader.into_dimensions() {
                    rows.push(h("Image"));
                    rows.push(r("Format", format));
                    rows.push(r("Size", format!("{w}x{hh}")));
                    rows.push(blank());
                }
            }
        } else if mime.starts_with("video/") || mime.starts_with("audio/") {
            rows.extend(media_rows(&f.url));
        }

        let time = |t: i64| {
            if t == 0 {
                "-".to_string()
            } else {
                full_time(t)
            }
        };
        rows.push(h("Base"));
        rows.push(r("Created", time(c.btime)));
        rows.push(r("Modified", time(c.mtime)));
        rows.push(r("Mimetype", mime.clone()));
        rows.push(blank());

        // yazi's plugin names for what handles this file; here they are
        // built in, but the names tell you which code path runs.
        let previewer = if f.is_dir() {
            "folder"
        } else if mime == "inode/empty" {
            "empty"
        } else if mime.starts_with("text/") || mime.contains("javascript") {
            "code"
        } else if mime.contains("json") {
            "json"
        } else if mime == "image/svg+xml" {
            "svg"
        } else if matches!(
            mime.as_str(),
            "image/avif" | "image/heic" | "image/heif" | "image/jxl"
        ) {
            "magick"
        } else if mime.starts_with("image/") {
            "image"
        } else if mime.starts_with("video/") {
            "video"
        } else if mime == "application/pdf" {
            "pdf"
        } else if mime.starts_with("font/") {
            "font"
        } else if mime.starts_with("application/") && crate::preview::is_archive(&mime) {
            "archive"
        } else {
            "file"
        };
        let spotter = match previewer {
            "folder" | "code" | "image" | "video" => previewer,
            "json" => "code",
            "svg" | "magick" => "image",
            _ => "file",
        };
        let preloader = match previewer {
            "image" | "svg" | "magick" | "video" | "pdf" | "font" => previewer,
            _ => "-",
        };
        rows.push(h("Plugins"));
        rows.push(r("Spotter", spotter.into()));
        rows.push(r("Previewer", previewer.into()));
        rows.push(r(
            "Fetchers",
            if f.is_dir() { "mime.dir" } else { "mime.local" }.into(),
        ));
        rows.push(r("Preloaders", preloader.into()));
        rows.push(blank());

        rows.push(h("Details"));
        if !f.is_dir() {
            rows.push(r(
                "Size",
                format!("{} ({} bytes)", human_size(c.len), c.len),
            ));
        }
        rows.push(r("Accessed", time(c.atime)));
        rows.push(r(
            "Mode",
            format!("{} ({:04o})", perm_string(c.mode), c.mode & 0o7777),
        ));
        rows.push(r(
            "Owner",
            format!("{}:{}", user_name(c.uid), group_name(c.gid)),
        ));
        rows.push(r("Links", c.nlink.to_string()));
        rows.push(r("Inode", c.ino.to_string()));
        if let Some(t) = &f.link_to {
            let broken = if c.is_orphan() { " (broken)" } else { "" };
            rows.push(r("Symlink", format!("{}{broken}", t.display())));
        }
        if (mime.starts_with("text/") || mime.contains("json") || mime.contains("javascript"))
            && c.len < 32 << 20
        {
            if let Ok(data) = std::fs::read(&f.url) {
                let lines = data.iter().filter(|&&b| b == b'\n').count();
                rows.push(r("Lines", lines.to_string()));
            }
        }
        rows.push(r("Path", f.url.to_string_lossy().into_owned()));
        rows
    }

    // ----------------------------------------------------------------------
    // Other layers
    // ----------------------------------------------------------------------

    fn tasks_cmd(&mut self, name: &str, _cmd: &Cmd) {
        let Some(v) = self.tasks_view.as_mut() else {
            if name == "show" {
                self.tasks_view = Some(TasksView { cursor: 0 });
            }
            return;
        };
        let n = self.tasks.len();
        match name {
            "close" => self.tasks_view = None,
            "arrow" => {
                let d: isize = match _cmd.arg(0) {
                    Some("prev") => -1,
                    Some("next") => 1,
                    Some(x) => x.parse().unwrap_or(0),
                    None => 0,
                };
                if n > 0 {
                    v.cursor = (v.cursor as isize + d).clamp(0, n as isize - 1) as usize;
                }
            }
            "cancel" => {
                // Newest first in the view.
                if let Some(t) = self.tasks.iter().rev().nth(v.cursor) {
                    t.cancel.store(true, Ordering::Relaxed);
                }
            }
            "inspect" => {
                if let Some(t) = self.tasks.iter().rev().nth(v.cursor) {
                    let mut text = format!("{}\n\n", t.title);
                    for l in &t.log {
                        text.push_str(l);
                        text.push('\n');
                    }
                    if let crate::tasks::State::Failed(e) = &t.state {
                        text.push_str(&format!("\nError: {e}\n"));
                    }
                    let tmp =
                        std::env::temp_dir().join(format!("wisp-task-{}.log", std::process::id()));
                    if std::fs::write(&tmp, text).is_ok() {
                        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".into());
                        self.fg = Some(Fg::Shell {
                            script: format!("{pager} {}", shell_quote(&tmp.to_string_lossy())),
                            args: Vec::new(),
                            cwd: self.cwd(),
                            wait_key: false,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn spot_cmd(&mut self, name: &str, cmd: &Cmd) {
        match name {
            "close" => self.spot = None,
            "arrow" => {
                if let Some(s) = self.spot.as_mut() {
                    let d: isize = match cmd.arg(0) {
                        Some("prev") => -1,
                        Some("next") => 1,
                        Some(x) => x.parse().unwrap_or(0),
                        None => 0,
                    };
                    // Step over section headers (rows without a value).
                    let n = s.rows.len() as isize;
                    let mut c = s.cursor as isize;
                    loop {
                        let next = c + d.signum();
                        if d == 0 || next < 0 || next >= n {
                            break;
                        }
                        c = next;
                        if !s.rows[c as usize].1.is_empty() {
                            s.cursor = c as usize;
                            break;
                        }
                    }
                }
            }
            "swipe" => {
                let d = match cmd.arg(0) {
                    Some("prev") => -1,
                    _ => 1,
                };
                self.tab_mut().current_mut().arrow(d);
                let cursor = self.spot.as_ref().map(|s| s.cursor).unwrap_or(0);
                self.spot_open();
                if let Some(s) = self.spot.as_mut() {
                    s.cursor = cursor.min(s.rows.len().saturating_sub(1));
                }
            }
            "copy" => {
                if let Some(s) = &self.spot {
                    if let Some((_, v)) = s.rows.get(s.cursor) {
                        let v = v.clone();
                        if let Err(e) = crate::clip::copy(&v) {
                            self.notify(Level::Error, "Copy", &e);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn pick_cmd(&mut self, name: &str, cmd: &Cmd) {
        let Some(p) = self.pick.as_mut() else { return };
        match name {
            "close" => {
                let p = self.pick.take().unwrap();
                if cmd.flag("submit") {
                    if let Some(o) = p.items.get(p.cursor).cloned() {
                        self.run_opener(&o, &p.targets);
                    }
                }
            }
            "arrow" => {
                let d: isize = match cmd.arg(0) {
                    Some("prev") => -1,
                    Some("next") => 1,
                    Some(x) => x.parse().unwrap_or(0),
                    None => 0,
                };
                let n = p.items.len() as isize;
                if n > 0 {
                    p.cursor = (p.cursor as isize + d).rem_euclid(n) as usize;
                }
            }
            _ => {}
        }
    }

    fn confirm_cmd(&mut self, name: &str, cmd: &Cmd) {
        match name {
            "close" => {
                let Some(c) = self.confirm.take() else { return };
                if matches!(c.kind, ConfirmKind::Plugin) {
                    return self.plugin_resume(crate::plugin::Resume::Bool(cmd.flag("submit")));
                }
                if !cmd.flag("submit") {
                    return;
                }
                match c.kind {
                    ConfirmKind::Trash(p) => self.remove_do(p, false),
                    ConfirmKind::Delete(p) => self.remove_do(p, true),
                    ConfirmKind::Quit { no_cwd } => {
                        for t in &self.tasks {
                            t.cancel.store(true, Ordering::Relaxed);
                        }
                        self.write_cwd = !no_cwd;
                        self.quit = true;
                    }
                    ConfirmKind::CreateOverwrite { path, dir } => {
                        let name = path.to_string_lossy().into_owned();
                        self.create_do(&name, dir, true);
                    }
                    ConfirmKind::RenameOverwrite { from, to } => {
                        let name = to.to_string_lossy().into_owned();
                        self.rename_do(from, &name, true);
                    }
                    ConfirmKind::Plugin => {}
                }
            }
            "arrow" => {
                if let Some(c) = self.confirm.as_mut() {
                    let d: isize = match cmd.arg(0) {
                        Some("prev") => -1,
                        Some("next") => 1,
                        Some(x) => x.parse().unwrap_or(0),
                        None => 0,
                    };
                    let n = c.list.len() as isize;
                    c.offset = (c.offset as isize + d).clamp(0, (n - 1).max(0)) as usize;
                }
            }
            _ => {}
        }
    }

    fn cmp_cmd(&mut self, name: &str, cmd: &Cmd) {
        let Some(i) = self.input.as_mut() else { return };
        match name {
            "close" => {
                if cmd.flag("submit") {
                    if let Some(c) = i.cmp.get(i.cmp_cursor).cloned() {
                        let v = i.input.value();
                        let base = match v.rfind('/') {
                            Some(k) => &v[..=k],
                            None => "",
                        };
                        let nv = format!("{base}{c}");
                        i.input.set_value(&nv);
                        i.input.mode = InputMode::Insert;
                        i.input.set_cursor(nv.chars().count());
                    }
                }
                i.cmp_shown = false;
                if cmd.flag("submit") {
                    self.on_input_change();
                    if let Some(i) = self.input.as_mut() {
                        // Keep the menu closed until the next edit.
                        i.cmp_shown = false;
                    }
                }
            }
            "arrow" => {
                let d: isize = match cmd.arg(0) {
                    Some("prev") => -1,
                    Some("next") => 1,
                    Some(x) => x.parse().unwrap_or(0),
                    None => 0,
                };
                let n = i.cmp.len() as isize;
                if n > 0 {
                    i.cmp_cursor = (i.cmp_cursor as isize + d).rem_euclid(n) as usize;
                }
            }
            _ => {}
        }
    }

    fn help_cmd(&mut self, name: &str, cmd: &Cmd) {
        match name {
            "escape" => {
                if let Some(h) = self.help.as_mut() {
                    if !h.filter.is_empty() {
                        h.filter.clear();
                        h.cursor = 0;
                        return;
                    }
                }
                self.help = None;
            }
            "close" => self.help = None,
            "arrow" => {
                if let Some(h) = self.help.as_mut() {
                    let d: isize = match cmd.arg(0) {
                        Some("prev") => -1,
                        Some("next") => 1,
                        Some(x) => x.parse().unwrap_or(0),
                        None => 0,
                    };
                    h.cursor = (h.cursor as isize + d).max(0) as usize;
                }
            }
            "filter" => {
                let cur = self
                    .help
                    .as_ref()
                    .map(|h| h.filter.clone())
                    .unwrap_or_default();
                self.open_input(
                    InputKind::HelpFilter,
                    "Filter:".into(),
                    &cur,
                    Origin::TopCenter,
                );
            }
            _ => {}
        }
    }

    fn input_cmd(&mut self, cmd: &Cmd) {
        let Some(i) = self.input.as_mut() else { return };
        let before = i.input.value();
        let done = i.input.exec(cmd);
        let changed = i.input.value() != before;
        match done {
            Some(submit) => self.close_input(submit),
            None => {
                if changed {
                    self.on_input_change();
                }
            }
        }
    }

    /// Live reactions to typing: filter/find as you type, path completion.
    pub fn on_input_change(&mut self) {
        let Some(i) = self.input.as_ref() else { return };
        let v = i.input.value();
        match i.kind.clone() {
            InputKind::Filter {
                smart, insensitive, ..
            } => {
                let p = Pattern::new(&v, smart, insensitive);
                self.tab_mut().current_mut().set_filter(p);
            }
            InputKind::Find {
                prev,
                smart,
                insensitive,
            } => {
                self.tab_mut().finder = Pattern::new(&v, smart, insensitive);
                self.find_arrow(prev, true);
            }
            InputKind::HelpFilter => {
                if let Some(h) = self.help.as_mut() {
                    h.filter = v;
                    h.cursor = 0;
                    h.offset = 0;
                }
            }
            InputKind::PluginRealtime => {
                self.plugin_resume(crate::plugin::Resume::Changed(v));
            }
            InputKind::Cd => {
                let list = complete(&v);
                if let Some(i) = self.input.as_mut() {
                    i.cmp_shown = !list.is_empty();
                    i.cmp = list;
                    i.cmp_cursor = 0;
                }
            }
            _ => {}
        }
    }

    fn close_input(&mut self, submit: bool) {
        let Some(st) = self.input.take() else { return };
        let v = st.input.value();
        match st.kind {
            InputKind::Cd => {
                if submit && !v.is_empty() {
                    let p = self.cwd().join(expand_path(&v));
                    self.cd(p);
                }
            }
            InputKind::Create { dir, force } => {
                if submit {
                    self.create_do(&v, dir, force);
                }
            }
            InputKind::Rename { from, force } => {
                if submit {
                    self.rename_do(from, &v, force);
                }
            }
            InputKind::Filter {
                smart,
                insensitive,
                prev,
            } => {
                let q = if submit { Some(v) } else { prev };
                let p = q.and_then(|q| Pattern::new(&q, smart, insensitive));
                self.tab_mut().current_mut().set_filter(p);
            }
            InputKind::Find { .. } => {
                if !submit {
                    self.tab_mut().finder = None;
                }
            }
            InputKind::Search { via } => {
                if submit {
                    self.search_do(&via, &v);
                }
            }
            InputKind::Shell { block, orphan } => {
                if submit && !v.trim().is_empty() {
                    self.shell_do(&v, block, orphan);
                }
            }
            InputKind::TabRename => {
                if submit {
                    self.tab_mut().name = (!v.is_empty()).then_some(v);
                }
            }
            InputKind::HelpFilter => {
                if let Some(h) = self.help.as_mut() {
                    if !submit {
                        h.filter.clear();
                    }
                }
            }
            InputKind::Plugin | InputKind::PluginRealtime => {
                self.plugin_resume(crate::plugin::Resume::Input(submit.then_some(v)));
            }
        }
    }
}

/// Path completions for an input like `~/Doc` → `["Documents/"]`.
fn complete(v: &str) -> Vec<String> {
    let (dir, prefix) = match v.rfind('/') {
        Some(k) => (&v[..=k], &v[k + 1..]),
        None => ("./", v),
    };
    let dirp = expand_path(dir);
    let Ok(rd) = std::fs::read_dir(&dirp) else {
        return Vec::new();
    };
    let lp = prefix.to_lowercase();
    let show_hidden = prefix.starts_with('.');
    let mut out: Vec<String> = rd
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') && !show_hidden {
                return None;
            }
            if !n.to_lowercase().starts_with(&lp) {
                return None;
            }
            let is_dir = e.path().is_dir();
            is_dir.then(|| format!("{n}/"))
        })
        .collect();
    out.sort_by(|a, b| crate::util::natural_cmp(a, b, false));
    out.truncate(50);
    if out.len() == 1 && out[0] == format!("{prefix}/") {
        return Vec::new();
    }
    out
}

/// Recursive size and entry count of a directory, computed in parallel.
pub fn dir_size(p: &Path) -> (u64, u64) {
    use std::sync::atomic::AtomicU64;
    let size = AtomicU64::new(0);
    let items = AtomicU64::new(0);
    let walker = ignore::WalkBuilder::new(p)
        .standard_filters(false)
        .follow_links(false)
        .threads(
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(12),
        )
        .build_parallel();
    walker.run(|| {
        let (size, items) = (&size, &items);
        Box::new(move |e| {
            if let Ok(e) = e {
                if e.depth() > 0 {
                    items.fetch_add(1, Ordering::Relaxed);
                    if let Ok(m) = e.metadata() {
                        if !m.is_dir() {
                            size.fetch_add(m.len(), Ordering::Relaxed);
                        }
                    }
                }
            }
            ignore::WalkState::Continue
        })
    });
    (size.into_inner(), items.into_inner())
}

/// The `Video` section of the spot table, from `ffprobe` (as yazi does).
fn media_rows(path: &Path) -> Vec<(String, String)> {
    if !on_path("ffprobe") {
        return Vec::new();
    }
    let Ok(out) = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-show_entries"])
        .arg("format=duration:stream=codec_name,codec_type,width,height")
        .args(["-of", "json"])
        .arg(path)
        .output()
    else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return Vec::new();
    };
    let dur: f64 = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0.0);
    let mut rows = vec![
        ("Video".to_string(), String::new()),
        (
            "  Duration:".to_string(),
            format!("{}:{:02}", (dur / 60.0) as u64, (dur % 60.0) as u64),
        ),
    ];
    for (i, s) in v["streams"].as_array().into_iter().flatten().enumerate() {
        let codec = s["codec_name"].as_str().unwrap_or("?").to_string();
        match s["codec_type"].as_str() {
            Some("video") => {
                rows.push((format!("  Stream {}:", i + 1), "video".into()));
                rows.push(("    Codec:".into(), codec));
                rows.push((
                    "    Size:".into(),
                    format!(
                        "{}x{}",
                        s["width"].as_u64().unwrap_or(0),
                        s["height"].as_u64().unwrap_or(0)
                    ),
                ));
            }
            Some("audio") => {
                rows.push((format!("  Stream {}:", i + 1), "audio".into()));
                rows.push(("    Codec:".into(), codec));
            }
            _ => {}
        }
    }
    rows.push((String::new(), String::new()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        let files = vec![PathBuf::from("/a/b c.txt"), PathBuf::from("/d/e.txt")];
        assert_eq!(
            expand_run("vi %s", &files, None),
            "vi '/a/b c.txt' /d/e.txt"
        );
        assert_eq!(
            expand_run("xdg-open %s1", &files, None),
            "xdg-open '/a/b c.txt'"
        );
        assert_eq!(expand_run("open %d1 100%%", &files, None), "open /a 100%");
        assert_eq!(expand_run("echo $EDITOR ñ", &files, None), "echo $EDITOR ñ");
    }

    #[test]
    fn completion() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("Documents")).unwrap();
        std::fs::create_dir(d.path().join("Downloads")).unwrap();
        std::fs::write(d.path().join("Dfile"), "").unwrap();
        let base = format!("{}/D", d.path().display());
        assert_eq!(complete(&base), vec!["Documents/", "Downloads/"]);
        let base = format!("{}/Doc", d.path().display());
        assert_eq!(complete(&base), vec!["Documents/"]);
    }

    #[test]
    fn sizes() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("a")).unwrap();
        std::fs::write(d.path().join("a/x"), "12345").unwrap();
        std::fs::write(d.path().join("y"), "123").unwrap();
        assert_eq!(dir_size(d.path()), (8, 3));
    }
}

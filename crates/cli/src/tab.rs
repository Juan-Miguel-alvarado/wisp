//! A tab: a working directory with its parent/preview folders, navigation
//! history, selection, visual mode, finder and search state.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::config::SortOpts;
use crate::folder::{Folder, Pattern};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Visual selection, anchored at an index.
    Select(usize),
    /// Visual un-selection, anchored at an index.
    Unset(usize),
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Select(_) => "SEL",
            Mode::Unset(_) => "UNS",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pref {
    pub sort: SortOpts,
    pub linemode: String,
    pub show_hidden: bool,
}

/// An in-progress or finished search, shown as a virtual folder.
pub struct Search {
    pub id: u64,
    pub query: String,
    pub folder: Folder,
    pub cancel: Arc<AtomicBool>,
    pub done: bool,
}

pub struct Tab {
    pub name: Option<String>,
    pub cwd: PathBuf,
    pub folders: HashMap<PathBuf, Folder>,
    pub back: Vec<PathBuf>,
    pub forward: Vec<PathBuf>,
    pub mode: Mode,
    pub selected: BTreeSet<PathBuf>,
    pub pref: Pref,
    pub finder: Option<Pattern>,
    pub search: Option<Search>,
}

impl Tab {
    pub fn new(cwd: PathBuf, pref: Pref) -> Self {
        let mut t = Self {
            name: None,
            cwd: PathBuf::new(),
            folders: HashMap::new(),
            back: Vec::new(),
            forward: Vec::new(),
            mode: Mode::Normal,
            selected: BTreeSet::new(),
            pref,
            finder: None,
            search: None,
        };
        t.set_cwd(cwd);
        t
    }

    /// The display name: custom, or the cwd's basename.
    pub fn title(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        self.cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.to_string_lossy().into_owned())
    }

    fn ensure(&mut self, p: &Path) {
        if !self.folders.contains_key(p) {
            self.folders
                .insert(p.to_path_buf(), Folder::new(p.to_path_buf()));
        }
    }

    /// Point the tab at `cwd` without touching history.
    pub fn set_cwd(&mut self, cwd: PathBuf) {
        self.ensure(&cwd);
        if let Some(p) = cwd.parent() {
            let p = p.to_path_buf();
            self.ensure(&p);
            if let Some(pf) = self.folders.get_mut(&p) {
                pf.hover(&cwd);
            }
        }
        self.cwd = cwd;
        self.mode = Mode::Normal;
        self.finder = None;
    }

    /// Change directory, recording history.
    pub fn cd(&mut self, to: PathBuf) -> bool {
        if to == self.cwd && self.search.is_none() {
            return false;
        }
        self.end_search();
        let from = std::mem::take(&mut self.cwd);
        self.back.push(from);
        if self.back.len() > 500 {
            self.back.remove(0);
        }
        self.forward.clear();
        self.set_cwd(to);
        true
    }

    pub fn back(&mut self) -> bool {
        let Some(to) = self.back.pop() else {
            return false;
        };
        self.end_search();
        let from = std::mem::take(&mut self.cwd);
        self.forward.push(from);
        self.set_cwd(to);
        true
    }

    pub fn forward(&mut self) -> bool {
        let Some(to) = self.forward.pop() else {
            return false;
        };
        self.end_search();
        let from = std::mem::take(&mut self.cwd);
        self.back.push(from);
        self.set_cwd(to);
        true
    }

    pub fn end_search(&mut self) {
        if let Some(s) = self.search.take() {
            s.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn current(&self) -> &Folder {
        if let Some(s) = &self.search {
            return &s.folder;
        }
        &self.folders[&self.cwd]
    }

    pub fn current_mut(&mut self) -> &mut Folder {
        if let Some(s) = &mut self.search {
            return &mut s.folder;
        }
        self.folders.get_mut(&self.cwd).expect("cwd folder exists")
    }

    pub fn parent(&self) -> Option<&Folder> {
        self.cwd.parent().and_then(|p| self.folders.get(p))
    }

    pub fn parent_path(&self) -> Option<PathBuf> {
        self.cwd.parent().map(Path::to_path_buf)
    }

    pub fn hovered(&self) -> Option<&crate::fs::File> {
        self.current().hovered()
    }

    /// The folder previewed in the right pane, if the hovered entry is a dir.
    pub fn hovered_dir(&self) -> Option<PathBuf> {
        self.hovered().filter(|f| f.is_dir()).map(|f| f.url.clone())
    }

    /// Selected paths, or the hovered file when nothing is selected.
    pub fn selected_or_hovered(&self) -> Vec<PathBuf> {
        if self.selected.is_empty() {
            self.hovered()
                .map(|f| vec![f.url.clone()])
                .unwrap_or_default()
        } else {
            self.selected.iter().cloned().collect()
        }
    }

    /// Apply visual-mode range to the selection when leaving it.
    pub fn visual_range(&self) -> Option<(usize, usize, bool)> {
        let cur = self.current().cursor;
        match self.mode {
            Mode::Normal => None,
            Mode::Select(a) => Some((a.min(cur), a.max(cur), true)),
            Mode::Unset(a) => Some((a.min(cur), a.max(cur), false)),
        }
    }

    pub fn commit_visual(&mut self) {
        if let Some((s, e, on)) = self.visual_range() {
            let urls: Vec<PathBuf> = (s..=e)
                .filter_map(|i| self.current().get(i).map(|f| f.url.clone()))
                .collect();
            for u in urls {
                if on {
                    self.selected.insert(u);
                } else {
                    self.selected.remove(&u);
                }
            }
        }
        self.mode = Mode::Normal;
    }

    /// Whether `idx` is inside the visual range (yazi's "marked" state).
    pub fn is_marked(&self, idx: usize) -> bool {
        matches!(self.visual_range(), Some((s, e, _)) if idx >= s && idx <= e)
    }

    /// Drop cached folders that are far from where we are, so long sessions
    /// do not grow without bound.
    pub fn trim_cache(&mut self, keep: &[PathBuf]) {
        if self.folders.len() <= 64 {
            return;
        }
        let cwd = self.cwd.clone();
        self.folders.retain(|p, _| {
            p == &cwd || cwd.starts_with(p) || p.starts_with(&cwd) || keep.contains(p)
        });
    }
}

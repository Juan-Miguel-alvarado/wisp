//! A loaded directory: its sorted entries, the visible subset (hidden files and
//! filter applied), and the cursor/scroll position.

use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};

use crate::config::SortOpts;
use crate::fs::{sort_files, File};

#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    Loading,
    Loaded,
    Failed(String),
}

/// A smart-case pattern: case-insensitive unless it contains an uppercase
/// letter. Invalid regexes fall back to a literal match.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub query: String,
    pub re: Regex,
}

impl Pattern {
    pub fn new(query: &str, smart: bool, insensitive: bool) -> Option<Self> {
        if query.is_empty() {
            return None;
        }
        let ci = insensitive || (smart && !query.chars().any(char::is_uppercase));
        let re = RegexBuilder::new(query)
            .case_insensitive(ci)
            .build()
            .or_else(|_| {
                RegexBuilder::new(&regex::escape(query))
                    .case_insensitive(ci)
                    .build()
            })
            .ok()?;
        Some(Self {
            query: query.to_string(),
            re,
        })
    }

    pub fn is_match(&self, s: &str) -> bool {
        self.re.is_match(s)
    }

    /// Byte ranges of all matches, for highlighting.
    pub fn highlights(&self, s: &str) -> Vec<(usize, usize)> {
        self.re
            .find_iter(s)
            .filter(|m| m.start() < m.end())
            .map(|m| (m.start(), m.end()))
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub items: Vec<File>,
    /// Indices into `items` of the visible entries.
    pub view: Vec<u32>,
    pub cursor: usize,
    pub offset: usize,
    pub stage: Stage,
    pub filter: Option<Pattern>,
    pub sorted: Option<SortOpts>,
    pub show_hidden: bool,
    /// The directory's own mtime when it was read, to detect staleness.
    pub dir_mtime: Option<(i64, u32)>,
    /// Bumped on every content change so caches keyed on it invalidate.
    pub revision: u64,
}

impl Folder {
    pub fn new(_cwd: PathBuf) -> Self {
        Self {
            items: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            stage: Stage::Loading,
            filter: None,
            sorted: None,
            show_hidden: false,
            dir_mtime: None,
            revision: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.view.len()
    }

    pub fn is_empty(&self) -> bool {
        self.view.is_empty()
    }

    pub fn get(&self, i: usize) -> Option<&File> {
        self.view.get(i).map(|&j| &self.items[j as usize])
    }

    pub fn files(&self) -> impl Iterator<Item = &File> + '_ {
        self.view.iter().map(move |&j| &self.items[j as usize])
    }

    pub fn hovered(&self) -> Option<&File> {
        self.get(self.cursor)
    }

    pub fn position(&self, url: &Path) -> Option<usize> {
        self.files().position(|f| f.url == url)
    }

    /// Install freshly read entries, keeping the cursor on the same file.
    pub fn set_items(
        &mut self,
        mut items: Vec<File>,
        sort: SortOpts,
        show_hidden: bool,
        presorted: bool,
    ) {
        let hovered = self.hovered().map(|f| f.url.clone());
        if !presorted {
            sort_files(&mut items, sort);
        }
        self.items = items;
        self.sorted = Some(sort);
        self.show_hidden = show_hidden;
        self.stage = Stage::Loaded;
        self.revision += 1;
        self.rebuild_view(hovered.as_deref());
    }

    pub fn set_failed(&mut self, err: String) {
        self.items.clear();
        self.view.clear();
        self.cursor = 0;
        self.offset = 0;
        self.stage = Stage::Failed(err);
        self.revision += 1;
    }

    /// Re-sort/re-filter in place if the preferences changed.
    pub fn apply_prefs(&mut self, sort: SortOpts, show_hidden: bool) {
        if self.stage != Stage::Loaded {
            return;
        }
        let hovered = self.hovered().map(|f| f.url.clone());
        let mut changed = false;
        if self.sorted != Some(sort) {
            sort_files(&mut self.items, sort);
            self.sorted = Some(sort);
            changed = true;
        }
        if self.show_hidden != show_hidden {
            self.show_hidden = show_hidden;
            changed = true;
        }
        if changed {
            self.revision += 1;
            self.rebuild_view(hovered.as_deref());
        }
    }

    pub fn set_filter(&mut self, filter: Option<Pattern>) {
        let hovered = self.hovered().map(|f| f.url.clone());
        self.filter = filter;
        self.revision += 1;
        self.rebuild_view(hovered.as_deref());
    }

    fn rebuild_view(&mut self, hovered: Option<&Path>) {
        let hidden = self.show_hidden;
        let filter = self.filter.as_ref();
        self.view = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, f)| hidden || !f.is_hidden())
            .filter(|(_, f)| filter.is_none_or(|p| p.is_match(&f.name)))
            .map(|(i, _)| i as u32)
            .collect();
        if let Some(h) = hovered {
            if let Some(i) = self.position(h) {
                self.cursor = i;
                return;
            }
        }
        self.cursor = self.cursor.min(self.len().saturating_sub(1));
    }

    /// Move the cursor by `step` (clamped). Returns whether it moved.
    pub fn arrow(&mut self, step: isize) -> bool {
        if self.is_empty() {
            return false;
        }
        let max = self.len() as isize - 1;
        let new = (self.cursor as isize + step).clamp(0, max) as usize;
        let moved = new != self.cursor;
        self.cursor = new;
        moved
    }

    pub fn arrow_to(&mut self, i: usize) -> bool {
        let new = i.min(self.len().saturating_sub(1));
        let moved = new != self.cursor;
        self.cursor = new;
        moved
    }

    pub fn hover(&mut self, url: &Path) -> bool {
        match self.position(url) {
            Some(i) => {
                self.cursor = i;
                true
            }
            None => false,
        }
    }

    /// Keep the cursor `scrolloff` rows away from the window edges.
    pub fn scroll_into_view(&mut self, height: usize, scrolloff: usize) {
        if height == 0 {
            return;
        }
        let so = scrolloff.min(height / 2);
        let len = self.len();
        if self.cursor < self.offset + so {
            self.offset = self.cursor.saturating_sub(so);
        } else if self.cursor + so + 1 > self.offset + height {
            self.offset = self.cursor + so + 1 - height;
        }
        self.offset = self.offset.min(len.saturating_sub(height));
    }

    /// The visible window `[offset, offset+height)`.
    pub fn window(&self, height: usize) -> std::ops::Range<usize> {
        let start = self.offset.min(self.len());
        start..(start + height).min(self.len())
    }

    /// Update one entry in place (after a rename/metadata change) or insert it.
    pub fn upsert(&mut self, file: File) {
        if let Some(i) = self.items.iter().position(|f| f.url == file.url) {
            self.items[i] = file;
        } else {
            self.items.push(file);
        }
        if let Some(s) = self.sorted {
            let h = self.hovered().map(|f| f.url.clone());
            sort_files(&mut self.items, s);
            self.revision += 1;
            self.rebuild_view(h.as_deref());
        }
    }

    pub fn remove(&mut self, url: &Path) {
        let h = self.hovered().map(|f| f.url.clone());
        self.items.retain(|f| f.url != url);
        self.revision += 1;
        self.rebuild_view(h.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SortBy;

    fn folder(names: &[&str]) -> Folder {
        let d = tempfile::tempdir().unwrap();
        for n in names {
            std::fs::write(d.path().join(n), "x").unwrap();
        }
        let items = crate::fs::read_dir(d.path()).unwrap();
        let mut f = Folder::new(d.path().to_path_buf());
        f.set_items(
            items,
            SortOpts {
                by: SortBy::Natural,
                sensitive: false,
                reverse: false,
                dir_first: true,
            },
            false,
            false,
        );
        std::mem::forget(d);
        f
    }

    #[test]
    fn hidden_and_filter() {
        let mut f = folder(&["a.rs", "b.txt", ".h", "c.rs"]);
        assert_eq!(f.len(), 3);
        f.set_filter(Pattern::new("rs", true, false));
        assert_eq!(f.len(), 2);
        f.set_filter(None);
        assert_eq!(f.len(), 3);
    }

    #[test]
    fn cursor_follows_file_across_resort() {
        let mut f = folder(&["a", "b", "c"]);
        f.arrow(2);
        assert_eq!(&*f.hovered().unwrap().name, "c");
        let mut s = f.sorted.unwrap();
        s.reverse = true;
        f.apply_prefs(s, false);
        assert_eq!(&*f.hovered().unwrap().name, "c");
        assert_eq!(f.cursor, 0);
    }

    #[test]
    fn scrolling() {
        let names: Vec<String> = (0..100).map(|i| format!("f{i:03}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut f = folder(&refs);
        f.arrow(50);
        f.scroll_into_view(20, 5);
        assert!(f.cursor + 5 < f.offset + 20 + 1);
        f.arrow_to(99);
        f.scroll_into_view(20, 5);
        assert_eq!(f.offset, 80);
    }
}

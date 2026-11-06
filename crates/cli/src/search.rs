//! Built-in recursive search, so `s`/`S` work without `fd` or `rg` installed
//! and without spawning a process: the same `ignore` walker fd uses (parallel,
//! `.gitignore`-aware) and ripgrep's own searcher for contents.
//!
//! Results stream back in batches so the list fills while the walk runs.
//! `via = "deep"` instead asks the persistent full-text index (PDF, DOCX,
//! code and text), ranked by relevance.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};

use crate::event::Msg;
use crate::folder::Pattern;
use crate::fs::File;

pub struct Opts {
    pub id: u64,
    pub via: String,
    pub root: PathBuf,
    pub query: String,
    pub hidden: bool,
    pub cancel: Arc<AtomicBool>,
}

/// A sink that records whether anything matched and stops at the first hit.
struct FirstMatch(bool);

impl Sink for FirstMatch {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, _: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.0 = true;
        Ok(false)
    }
}

struct Batch {
    files: Vec<File>,
    last: Instant,
}

fn flush(b: &Mutex<Batch>, tx: &Sender<Msg>, id: u64, force: bool) {
    let mut g = b.lock().unwrap();
    if g.files.is_empty() {
        return;
    }
    if force || g.files.len() >= 256 || g.last.elapsed() > Duration::from_millis(60) {
        let files = std::mem::take(&mut g.files);
        g.last = Instant::now();
        let _ = tx.send(Msg::Search {
            id,
            files,
            done: false,
            error: None,
        });
    }
}

pub fn spawn(o: Opts, tx: Sender<Msg>) {
    std::thread::spawn(move || {
        let id = o.id;
        let error = match o.via.as_str() {
            "deep" => deep(&o, &tx),
            "rg" | "content" => contents(&o, &tx),
            _ => names(&o, &tx),
        };
        let _ = tx.send(Msg::Search {
            id,
            files: Vec::new(),
            done: true,
            error,
        });
    });
}

fn walker(o: &Opts) -> WalkBuilder {
    let mut w = WalkBuilder::new(&o.root);
    w.hidden(!o.hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(true)
        .follow_links(false)
        .threads(
            std::thread::available_parallelism()
                .map_or(4, |n| n.get())
                .min(12),
        );
    w
}

fn names(o: &Opts, tx: &Sender<Msg>) -> Option<String> {
    let pat = match Pattern::new(&o.query, true, false) {
        Some(p) => p,
        None => return Some("empty query".into()),
    };
    let batch = Arc::new(Mutex::new(Batch {
        files: Vec::new(),
        last: Instant::now(),
    }));
    let root = o.root.clone();
    walker(o).build_parallel().run(|| {
        let (pat, batch, tx, cancel, root) = (
            pat.clone(),
            batch.clone(),
            tx.clone(),
            o.cancel.clone(),
            root.clone(),
        );
        Box::new(move |e| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(e) = e else { return WalkState::Continue };
            if e.path() == root {
                return WalkState::Continue;
            }
            let name = e.file_name().to_string_lossy();
            if pat.is_match(&name) {
                if let Ok(f) = File::from_path(e.path()) {
                    batch.lock().unwrap().files.push(f);
                    flush(&batch, &tx, o.id, false);
                }
            }
            WalkState::Continue
        })
    });
    flush(&batch, tx, o.id, true);
    None
}

fn contents(o: &Opts, tx: &Sender<Msg>) -> Option<String> {
    let ci = !o.query.chars().any(char::is_uppercase);
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(ci)
        .build(&o.query)
        .or_else(|_| {
            RegexMatcherBuilder::new()
                .case_insensitive(ci)
                .build(&regex::escape(&o.query))
        }) {
        Ok(m) => m,
        Err(e) => return Some(e.to_string()),
    };
    let batch = Arc::new(Mutex::new(Batch {
        files: Vec::new(),
        last: Instant::now(),
    }));
    walker(o).build_parallel().run(|| {
        let (matcher, batch, tx, cancel) =
            (matcher.clone(), batch.clone(), tx.clone(), o.cancel.clone());
        let mut searcher = SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .line_number(false)
            .build();
        Box::new(move |e| {
            if cancel.load(Ordering::Relaxed) {
                return WalkState::Quit;
            }
            let Ok(e) = e else { return WalkState::Continue };
            if !e.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            let mut sink = FirstMatch(false);
            let _ = searcher.search_path(&matcher, e.path(), &mut sink);
            let hit = sink.0;
            if hit {
                if let Ok(f) = File::from_path(e.path()) {
                    batch.lock().unwrap().files.push(f);
                    flush(&batch, &tx, o.id, false);
                }
            }
            WalkState::Continue
        })
    });
    flush(&batch, tx, o.id, true);
    None
}

/// Ranked search over the persistent full-text index (built by
/// `wisp index`). Results are limited to `root`.
fn deep(o: &Opts, tx: &Sender<Msg>) -> Option<String> {
    use wisp_core::{DeepSearch, QueryOptions};
    let ds = match DeepSearch::load(None) {
        Ok(d) => d,
        Err(wisp_core::DeepSearchError::NotFound(_)) => {
            return Some("No index yet: run `wisp index` (or `wisp index <dir>`) first".into())
        }
        Err(wisp_core::DeepSearchError::VersionMismatch { .. }) => {
            return Some("The index is from an older version: run `wisp index` again".into())
        }
        Err(e) => return Some(format!("Could not read the index: {e}")),
    };
    let opts = QueryOptions {
        limit: 500,
        ..QueryOptions::default()
    };
    let files: Vec<File> = ds
        .search(&o.query, &opts)
        .into_iter()
        .filter(|r| r.path.starts_with(&o.root))
        .filter_map(|r| File::from_path(&r.path).ok())
        .collect();
    let _ = tx.send(Msg::Search {
        id: o.id,
        files,
        done: false,
        error: None,
    });
    None
}

/// Paths relative to the search root, for the `prefix/` shown before names.
pub fn prefix_of<'a>(root: &Path, file: &'a Path) -> Option<std::borrow::Cow<'a, str>> {
    let parent = file.parent()?;
    let rel = parent.strip_prefix(root).ok()?;
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(via: &str, root: &Path, q: &str) -> Vec<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(
            Opts {
                id: 7,
                via: via.into(),
                root: root.to_path_buf(),
                query: q.into(),
                hidden: false,
                cancel: Arc::new(AtomicBool::new(false)),
            },
            tx,
        );
        let mut out = Vec::new();
        loop {
            if let Msg::Search { files, done, .. } = rx.recv().unwrap() {
                out.extend(files.into_iter().map(|f| f.name.to_string()));
                if done {
                    break;
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn finds_names_and_contents() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("x/y")).unwrap();
        std::fs::write(d.path().join("x/y/Needle.txt"), "nothing").unwrap();
        std::fs::write(d.path().join("x/hay.txt"), "a needle in here").unwrap();
        std::fs::write(d.path().join(".hidden_needle"), "needle").unwrap();
        assert_eq!(collect("fd", d.path(), "needle"), vec!["Needle.txt"]);
        assert_eq!(collect("rg", d.path(), "needle"), vec!["hay.txt"]);
        assert_eq!(collect("fd", d.path(), "Needle"), vec!["Needle.txt"]);
    }
}

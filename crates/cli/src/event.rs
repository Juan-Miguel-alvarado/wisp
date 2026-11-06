//! The app's single event channel, and the terminal input thread.
//!
//! Everything that can change the screen — key presses, finished directory
//! reads, previews, file-watcher hits, task progress, search results — arrives
//! as a [`Msg`] on one channel. The main loop blocks on it, so an idle
//! file manager uses no CPU at all.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event};

use crate::config::SortOpts;
use crate::fs::File;
use crate::preview::PreviewContent;
use crate::tasks::TaskEvent;

/// A directory read: entries plus the directory's own mtime, or an error.
pub type Listing = Result<(Vec<File>, Option<(i64, u32)>), String>;

pub enum Msg {
    Input(Event),
    Loaded {
        path: PathBuf,
        result: Listing,
        sort: SortOpts,
    },
    Preview {
        key: crate::preview::Key,
        content: PreviewContent,
    },
    Watch(Vec<PathBuf>),
    Task(TaskEvent),
    Search {
        id: u64,
        files: Vec<File>,
        done: bool,
        error: Option<String>,
    },
    Mimes(Vec<(PathBuf, String)>),
    DirSize {
        path: PathBuf,
        size: u64,
        items: u64,
    },
    /// A command sent with `wisp emit`.
    Remote(String),
}

/// Reads terminal events on its own thread. It can be paused while a
/// foreground program (an editor, `fzf`, a shell) owns the terminal, or while
/// the app queries the terminal directly.
pub struct InputThread {
    paused: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl InputThread {
    pub fn spawn(tx: Sender<Msg>) -> Self {
        let paused = Arc::new(AtomicBool::new(false));
        let parked = Arc::new(AtomicBool::new(false));
        let (p, k) = (paused.clone(), parked.clone());
        std::thread::spawn(move || loop {
            if p.load(Ordering::Acquire) {
                k.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            k.store(false, Ordering::Release);
            match event::poll(Duration::from_millis(30)) {
                Ok(true) => {
                    // Re-check: we may have been paused while polling.
                    if p.load(Ordering::Acquire) {
                        continue;
                    }
                    match event::read() {
                        Ok(ev) => {
                            if tx.send(Msg::Input(ev)).is_err() {
                                return;
                            }
                        }
                        Err(_) => return,
                    }
                }
                Ok(false) => {}
                Err(_) => return,
            }
        });
        Self { paused, parked }
    }

    /// Stop reading stdin and wait until the thread has let go of it.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
        for _ in 0..200 {
            if self.parked.load(Ordering::Acquire) {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
    }
}

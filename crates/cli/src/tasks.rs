//! Background tasks: copying, moving, linking, trashing, deleting, and
//! running processes. Each task runs on its own thread, reports progress over
//! the app channel, and can be cancelled.

use std::fs;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::event::Msg;
use crate::util::{relative_path, unique_name};

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

/// What sort of work a task is, for its icon and progress display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Copy,
    Cut,
    Delete,
    Other,
}

#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub kind: Kind,
    pub id: u64,
    pub title: String,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub items_total: u64,
    pub items_done: u64,
    pub state: State,
    pub cancel: Arc<AtomicBool>,
    pub log: Vec<String>,
}

impl TaskInfo {
    pub fn percent(&self) -> u16 {
        if self.bytes_total > 0 {
            ((self.bytes_done as f64 / self.bytes_total as f64) * 100.0).min(100.0) as u16
        } else if self.items_total > 0 {
            ((self.items_done as f64 / self.items_total as f64) * 100.0).min(100.0) as u16
        } else {
            0
        }
    }
}

#[derive(Debug)]
pub enum TaskEvent {
    Progress {
        id: u64,
        bytes_done: u64,
        bytes_total: u64,
        items_done: u64,
        items_total: u64,
    },
    Log {
        id: u64,
        line: String,
    },
    Finished {
        id: u64,
        result: Result<(), String>,
        /// Directories whose listing changed.
        touched: Vec<PathBuf>,
        cancelled: bool,
    },
}

/// What a task does.
#[derive(Clone, Debug)]
pub enum Job {
    Copy {
        srcs: Vec<PathBuf>,
        dst_dir: PathBuf,
        force: bool,
        follow: bool,
    },
    Move {
        srcs: Vec<PathBuf>,
        dst_dir: PathBuf,
        force: bool,
    },
    Link {
        srcs: Vec<PathBuf>,
        dst_dir: PathBuf,
        relative: bool,
        force: bool,
    },
    Hardlink {
        srcs: Vec<PathBuf>,
        dst_dir: PathBuf,
        force: bool,
    },
    Trash {
        paths: Vec<PathBuf>,
    },
    Delete {
        paths: Vec<PathBuf>,
    },
    Process {
        /// What the task list shows, e.g. `Background command: make`.
        title: String,
        program: String,
        args: Vec<String>,
        cwd: PathBuf,
        orphan: bool,
    },
    Extract {
        archive: PathBuf,
        dst_dir: PathBuf,
    },
}

impl Job {
    pub fn title(&self) -> String {
        fn names(v: &[PathBuf]) -> String {
            match v {
                [one] => one
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                _ => format!("{} items", v.len()),
            }
        }
        match self {
            Job::Copy { srcs, dst_dir, .. } => {
                format!("Copy {} to {}", names(srcs), dst_dir.display())
            }
            Job::Move { srcs, dst_dir, .. } => {
                format!("Move {} to {}", names(srcs), dst_dir.display())
            }
            Job::Link { srcs, dst_dir, .. } => {
                format!("Link {} to {}", names(srcs), dst_dir.display())
            }
            Job::Hardlink { srcs, dst_dir, .. } => {
                format!("Hardlink {} to {}", names(srcs), dst_dir.display())
            }
            Job::Trash { paths } => format!("Trash {}", names(paths)),
            Job::Delete { paths } => format!("Delete {}", names(paths)),
            Job::Extract { archive, .. } => format!(
                "Extract {}",
                archive
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            Job::Process { title, .. } => title.clone(),
        }
    }
}

struct Ctx {
    id: u64,
    tx: Sender<Msg>,
    cancel: Arc<AtomicBool>,
    bytes_total: u64,
    bytes_done: u64,
    items_total: u64,
    items_done: u64,
    last: Instant,
}

impl Ctx {
    fn cancelled(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"))
        } else {
            Ok(())
        }
    }

    fn report(&mut self, force: bool) {
        if force || self.last.elapsed() > Duration::from_millis(100) {
            self.last = Instant::now();
            let _ = self.tx.send(Msg::Task(TaskEvent::Progress {
                id: self.id,
                bytes_done: self.bytes_done,
                bytes_total: self.bytes_total,
                items_done: self.items_done,
                items_total: self.items_total,
            }));
        }
    }
}

/// Total size and item count of a tree (not following symlinks).
fn measure(p: &Path, bytes: &mut u64, items: &mut u64) {
    let Ok(m) = fs::symlink_metadata(p) else {
        return;
    };
    *items += 1;
    if m.is_dir() {
        if let Ok(rd) = fs::read_dir(p) {
            for e in rd.flatten() {
                measure(&e.path(), bytes, items);
            }
        }
    } else {
        *bytes += m.len();
    }
}

fn copy_file(src: &Path, dst: &Path, ctx: &mut Ctx) -> io::Result<()> {
    let mut from = fs::File::open(src)?;
    let meta = from.metadata()?;
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    // `io::copy` between files uses copy_file_range on Linux; chunking keeps
    // progress and cancellation responsive on huge files.
    loop {
        ctx.cancelled()?;
        let n = io::copy(&mut (&mut from).take(16 << 20), &mut to)?;
        if n == 0 {
            break;
        }
        ctx.bytes_done += n;
        ctx.report(false);
    }
    to.set_permissions(meta.permissions())?;
    let _ = to.set_modified(meta.modified()?);
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path, follow: bool, ctx: &mut Ctx) -> io::Result<()> {
    ctx.cancelled()?;
    let meta = if follow {
        fs::metadata(src)?
    } else {
        fs::symlink_metadata(src)?
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        if fs::symlink_metadata(dst).is_ok() {
            fs::remove_file(dst)?;
        }
        std::os::unix::fs::symlink(target, dst)?;
    } else if meta.is_dir() {
        match fs::create_dir(dst) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && dst.is_dir() => {}
            Err(e) => return Err(e),
        }
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()), follow, ctx)?;
        }
        let _ = fs::set_permissions(dst, meta.permissions());
    } else {
        copy_file(src, dst, ctx)?;
    }
    ctx.items_done += 1;
    ctx.report(false);
    Ok(())
}

fn hardlink_tree(src: &Path, dst: &Path, ctx: &mut Ctx) -> io::Result<()> {
    ctx.cancelled()?;
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        match fs::create_dir(dst) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        for e in fs::read_dir(src)? {
            let e = e?;
            hardlink_tree(&e.path(), &dst.join(e.file_name()), ctx)?;
        }
    } else {
        if fs::symlink_metadata(dst).is_ok() {
            fs::remove_file(dst)?;
        }
        fs::hard_link(src, dst)?;
    }
    ctx.items_done += 1;
    ctx.report(false);
    Ok(())
}

/// Where `src` lands inside `dst_dir`, resolving name conflicts.
fn target(src: &Path, dst_dir: &Path, force: bool) -> io::Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let dst = dst_dir.join(name);
    if dst.starts_with(src) && src.is_dir() && !fs::symlink_metadata(src)?.is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("cannot put {} inside itself", src.display()),
        ));
    }
    if dst == src {
        // Pasting into the same directory: always make a copy beside it.
        return Ok(unique_name(&dst));
    }
    if force {
        Ok(dst)
    } else {
        Ok(unique_name(&dst))
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(p)?;
    if m.is_dir() {
        fs::remove_dir_all(p)
    } else {
        fs::remove_file(p)
    }
}

fn parents(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = paths
        .iter()
        .filter_map(|p| p.parent().map(Path::to_path_buf))
        .collect();
    v.sort();
    v.dedup();
    v
}

fn run_job(job: Job, ctx: &mut Ctx) -> (io::Result<()>, Vec<PathBuf>) {
    match job {
        Job::Copy {
            srcs,
            dst_dir,
            force,
            follow,
        } => {
            for s in &srcs {
                measure(s, &mut ctx.bytes_total, &mut ctx.items_total);
            }
            ctx.report(true);
            let r = (|| {
                for s in &srcs {
                    let dst = target(s, &dst_dir, force)?;
                    if force && dst.exists() && !dst.is_dir() {
                        remove_any(&dst)?;
                    }
                    copy_tree(s, &dst, follow, ctx)?;
                }
                Ok(())
            })();
            (r, vec![dst_dir])
        }
        Job::Move {
            srcs,
            dst_dir,
            force,
        } => {
            ctx.items_total = srcs.len() as u64;
            let mut touched = parents(&srcs);
            touched.push(dst_dir.clone());
            let r = (|| {
                for s in &srcs {
                    ctx.cancelled()?;
                    if s.parent() == Some(dst_dir.as_path()) {
                        ctx.items_done += 1;
                        continue; // moving into the same place is a no-op
                    }
                    let dst = target(s, &dst_dir, force)?;
                    if force && fs::symlink_metadata(&dst).is_ok() {
                        remove_any(&dst)?;
                    }
                    match fs::rename(s, &dst) {
                        Ok(()) => {}
                        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                            // Different filesystem: copy, then remove.
                            let (mut b, mut i) = (0, 0);
                            measure(s, &mut b, &mut i);
                            ctx.bytes_total += b;
                            copy_tree(s, &dst, false, ctx)?;
                            remove_any(s)?;
                        }
                        Err(e) => return Err(e),
                    }
                    ctx.items_done += 1;
                    ctx.report(false);
                }
                Ok(())
            })();
            (r, touched)
        }
        Job::Link {
            srcs,
            dst_dir,
            relative,
            force,
        } => {
            ctx.items_total = srcs.len() as u64;
            let r = (|| {
                for s in &srcs {
                    let dst = target(s, &dst_dir, force)?;
                    if force && fs::symlink_metadata(&dst).is_ok() {
                        remove_any(&dst)?;
                    }
                    let src = if relative {
                        relative_path(&dst_dir, s)
                    } else {
                        s.clone()
                    };
                    std::os::unix::fs::symlink(src, &dst)?;
                    ctx.items_done += 1;
                }
                Ok(())
            })();
            (r, vec![dst_dir])
        }
        Job::Hardlink {
            srcs,
            dst_dir,
            force,
        } => {
            for s in &srcs {
                let mut b = 0;
                measure(s, &mut b, &mut ctx.items_total);
            }
            let r = (|| {
                for s in &srcs {
                    let dst = target(s, &dst_dir, force)?;
                    hardlink_tree(s, &dst, ctx)?;
                }
                Ok(())
            })();
            (r, vec![dst_dir])
        }
        Job::Trash { paths } => {
            ctx.items_total = paths.len() as u64;
            ctx.report(true);
            let touched = parents(&paths);
            let r = trash::delete_all(&paths).map_err(|e| io::Error::other(e.to_string()));
            ctx.items_done = ctx.items_total;
            (r, touched)
        }
        Job::Delete { paths } => {
            for p in &paths {
                let mut b = 0;
                measure(p, &mut b, &mut ctx.items_total);
            }
            ctx.report(true);
            let touched = parents(&paths);
            let r = (|| {
                for p in &paths {
                    ctx.cancelled()?;
                    remove_any(p)?;
                    let (mut b, mut i) = (0, 0);
                    measure(p, &mut b, &mut i);
                    ctx.items_done = ctx.items_total.min(ctx.items_done + 1);
                    ctx.report(false);
                }
                Ok(())
            })();
            ctx.items_done = ctx.items_total;
            (r, touched)
        }
        Job::Process {
            program,
            args,
            cwd,
            orphan,
            ..
        } => {
            let r = run_process(&program, &args, &cwd, orphan, ctx);
            (r, vec![cwd])
        }
        Job::Extract { archive, dst_dir } => {
            let r = extract(&archive, &dst_dir, ctx);
            (r, vec![dst_dir])
        }
    }
}

/// Extract an archive into a fresh directory named after it (`a.tar.gz` →
/// `a/`, or `a_1/` if taken), using whichever extractor is installed. A
/// single top-level entry is moved up so `x.zip` containing `x/` does not
/// end up as `x/x/`.
fn extract(archive: &Path, dst_dir: &Path, ctx: &mut Ctx) -> io::Result<()> {
    let name = archive
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    let mut stem = name.as_str();
    for ext in [
        ".tar.gz", ".tar.xz", ".tar.zst", ".tar.bz2", ".tgz", ".txz", ".tbz2",
    ] {
        if let Some(s) = name.strip_suffix(ext) {
            stem = s;
            break;
        }
    }
    if stem == name {
        stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(&name);
    }
    let tmp = dst_dir.join(format!(".{stem}.extracting-{}", std::process::id()));
    fs::create_dir_all(&tmp)?;
    let on_path = crate::util::on_path;
    let lower = name.to_ascii_lowercase();
    let (program, args): (&str, Vec<String>) = if on_path("7z") {
        (
            "7z",
            vec![
                "x".into(),
                "-y".into(),
                format!("-o{}", tmp.display()),
                archive.to_string_lossy().into_owned(),
            ],
        )
    } else if lower.ends_with(".zip") && on_path("unzip") {
        (
            "unzip",
            vec![
                "-q".into(),
                archive.to_string_lossy().into_owned(),
                "-d".into(),
                tmp.to_string_lossy().into_owned(),
            ],
        )
    } else if on_path("bsdtar") {
        (
            "bsdtar",
            vec![
                "-xf".into(),
                archive.to_string_lossy().into_owned(),
                "-C".into(),
                tmp.to_string_lossy().into_owned(),
            ],
        )
    } else {
        (
            "tar",
            vec![
                "-xf".into(),
                archive.to_string_lossy().into_owned(),
                "-C".into(),
                tmp.to_string_lossy().into_owned(),
            ],
        )
    };
    let r = run_process(program, &args, dst_dir, false, ctx);
    if let Err(e) = r {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let entries: Vec<_> = fs::read_dir(&tmp)?.flatten().collect();
    if entries.len() == 1 {
        let only = entries[0].path();
        let target = unique_name(&dst_dir.join(entries[0].file_name()));
        fs::rename(&only, &target)?;
        fs::remove_dir(&tmp)?;
    } else {
        let target = unique_name(&dst_dir.join(stem));
        fs::rename(&tmp, &target)?;
    }
    Ok(())
}

fn run_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    orphan: bool,
    ctx: &mut Ctx,
) -> io::Result<()> {
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(cwd).stdin(Stdio::null());
    if orphan {
        use std::os::unix::process::CommandExt;
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()?;
        return Ok(());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let tx = ctx.tx.clone();
    let id = ctx.id;
    let pump = move |r: Box<dyn Read + Send>| {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for line in io::BufReader::new(r).lines().map_while(Result::ok) {
                let _ = tx.send(Msg::Task(TaskEvent::Log { id, line }));
            }
        })
    };
    let h1 = out.map(|o| pump(Box::new(o)));
    let h2 = err.map(|e| pump(Box::new(e)));
    loop {
        if ctx.cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
        }
        if let Some(status) = child.try_wait()? {
            if let Some(h) = h1 {
                let _ = h.join();
            }
            if let Some(h) = h2 {
                let _ = h.join();
            }
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!("exited with {status}")))
            };
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Start `job` on a worker thread.
pub fn spawn(id: u64, job: Job, tx: Sender<Msg>) -> TaskInfo {
    let cancel = Arc::new(AtomicBool::new(false));
    let kind = match &job {
        Job::Copy { .. } | Job::Link { .. } | Job::Hardlink { .. } => Kind::Copy,
        Job::Move { .. } => Kind::Cut,
        Job::Trash { .. } | Job::Delete { .. } => Kind::Delete,
        _ => Kind::Other,
    };
    let info = TaskInfo {
        kind,
        id,
        title: job.title(),
        bytes_total: 0,
        bytes_done: 0,
        items_total: 0,
        items_done: 0,
        state: State::Running,
        cancel: cancel.clone(),
        log: Vec::new(),
    };
    std::thread::spawn(move || {
        let mut ctx = Ctx {
            id,
            tx: tx.clone(),
            cancel,
            bytes_total: 0,
            bytes_done: 0,
            items_total: 0,
            items_done: 0,
            last: Instant::now(),
        };
        let (r, touched) = run_job(job, &mut ctx);
        ctx.report(true);
        let cancelled = ctx.cancel.load(Ordering::Relaxed);
        let _ = tx.send(Msg::Task(TaskEvent::Finished {
            id,
            result: r.map_err(|e| e.to_string()),
            touched,
            cancelled,
        }));
    });
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(job: Job) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(1, job, tx);
        loop {
            if let Msg::Task(TaskEvent::Finished { result, .. }) = rx.recv().unwrap() {
                return result;
            }
        }
    }

    #[test]
    fn copy_move_link_delete() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        fs::create_dir_all(a.join("sub")).unwrap();
        fs::write(a.join("sub/f.txt"), "hello").unwrap();
        fs::create_dir(&b).unwrap();

        run(Job::Copy {
            srcs: vec![a.join("sub")],
            dst_dir: b.clone(),
            force: false,
            follow: false,
        })
        .unwrap();
        assert_eq!(fs::read_to_string(b.join("sub/f.txt")).unwrap(), "hello");

        // Conflict without force → renamed copy.
        run(Job::Copy {
            srcs: vec![a.join("sub")],
            dst_dir: b.clone(),
            force: false,
            follow: false,
        })
        .unwrap();
        assert!(b.join("sub_1/f.txt").exists());

        // Copy into itself is refused.
        assert!(run(Job::Copy {
            srcs: vec![a.clone()],
            dst_dir: a.join("sub"),
            force: false,
            follow: false,
        })
        .is_err());

        run(Job::Move {
            srcs: vec![b.join("sub_1")],
            dst_dir: a.clone(),
            force: false,
        })
        .unwrap();
        assert!(a.join("sub_1/f.txt").exists() && !b.join("sub_1").exists());

        run(Job::Link {
            srcs: vec![a.join("sub/f.txt")],
            dst_dir: b.clone(),
            relative: true,
            force: false,
        })
        .unwrap();
        assert_eq!(
            fs::read_link(b.join("f.txt")).unwrap(),
            PathBuf::from("../a/sub/f.txt")
        );
        assert_eq!(fs::read_to_string(b.join("f.txt")).unwrap(), "hello");

        run(Job::Delete {
            paths: vec![a.clone()],
        })
        .unwrap();
        assert!(!a.exists());
    }
}

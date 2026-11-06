//! `wisp` — a blazing-fast terminal file manager with yazi's UI, keymap
//! and config format, plus a persistent full-text index for deep search.
//!
//! ```text
//! wisp [ENTRY]            open the file manager (ENTRY: dir, or file to reveal)
//!   --cwd-file <FILE>          write the last directory to FILE on exit
//!   --chooser-file <FILE>      file-picker mode: write the chosen paths and exit
//! wisp index [PATH]      build the full-text index used by deep search (F)
//! wisp query "<q>"       one-shot ranked search over the index
//! wisp stats             report on the index
//! ```

mod app;
mod clip;
mod commands;
mod config;
mod event;
mod folder;
mod fs;
mod glob;
mod input;
mod keymap;
mod pkg;
mod plugin;
mod preview;
mod remote;
mod search;
mod tab;
mod tasks;
mod theme;
mod ui;
mod util;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use wisp_core::{DeepSearch, IndexOptions, Progress, QueryOptions};

#[derive(Parser)]
#[command(
    name = "wisp",
    version,
    about = "A blazing-fast terminal file manager (yazi-compatible) with deep full-text search",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Directory to open, or a file to reveal.
    entry: Option<PathBuf>,

    /// Write the cwd on exit to this file (for `cd`-on-quit shell wrappers).
    #[arg(long)]
    cwd_file: Option<PathBuf>,

    /// File-chooser mode: write the chosen paths to this file and exit.
    #[arg(long)]
    chooser_file: Option<PathBuf>,

    /// Clear the preview/thumbnail cache and exit.
    #[arg(long)]
    clear_cache: bool,

    /// Print where configuration is read from and exit.
    #[arg(long)]
    debug: bool,

    /// Path to the full-text index cache (default: ~/.cache/wisp/index.bin).
    #[arg(long, global = true)]
    cache: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Build or update the full-text index used by deep search.
    Index {
        /// Root directory to index (default: your home directory).
        path: Option<PathBuf>,
        /// Reindex only changed files instead of a full rebuild.
        #[arg(long)]
        incremental: bool,
    },
    /// Run a one-shot query against the index and print ranked results.
    Query {
        #[arg(required = true)]
        query: Vec<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Print statistics about the index.
    Stats,
    /// Install and manage yazi plugins/flavors (like `ya pkg`):
    /// `add owner/repo[:name]`, `list`, `upgrade`, `delete <name>`.
    Pkg {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a command to the running instance (like `ya emit`), e.g.
    /// `wisp emit cd ~/Downloads`.
    Emit {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Send a command to a specific instance by id (like `ya emit-to`).
    EmitTo {
        id: u32,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Tests that change process-wide environment variables hold this.
#[cfg(test)]
pub static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Startup tracing for profiling (`DS_TRACE=1`), printed to stderr.
pub fn trace(what: &str) {
    static PATH: std::sync::OnceLock<Option<std::ffi::OsString>> = std::sync::OnceLock::new();
    if let Some(path) = PATH.get_or_init(|| std::env::var_os("DS_TRACE")) {
        if let Some(s) = START.get() {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(f, "{:>10.2?} {what}", s.elapsed());
            }
        }
    }
}

fn main() -> Result<()> {
    START.get_or_init(std::time::Instant::now);
    // The common case — plain `wisp` — skips building the clap parser.
    if std::env::args_os().len() == 1 {
        return run_fm(None, None, None);
    }
    let cli = Cli::parse();
    trace("args");
    let cache = cli.cache.as_deref();
    match cli.cmd {
        Some(Command::Index { path, incremental }) => cmd_index(cache, path, incremental),
        Some(Command::Query { query, limit, json }) => {
            cmd_query(cache, &query.join(" "), limit, json)
        }
        Some(Command::Stats) => cmd_stats(cache),
        Some(Command::Pkg { args }) => pkg::run(&args),
        Some(Command::Emit { args }) => remote::emit(&args, None),
        Some(Command::EmitTo { id, args }) => remote::emit(&args, Some(id)),
        None => {
            if cli.clear_cache {
                let d = config::cache_dir();
                let mut n = 0;
                if let Ok(rd) = std::fs::read_dir(&d) {
                    for e in rd.flatten() {
                        if e.path().extension().is_some_and(|x| x == "jpg") {
                            let _ = std::fs::remove_file(e.path());
                            n += 1;
                        }
                    }
                }
                println!("Removed {n} cached thumbnails from {}", d.display());
                return Ok(());
            }
            if cli.debug {
                return debug_info();
            }
            run_fm(cli.entry, cli.cwd_file, cli.chooser_file)
        }
    }
}

fn run_fm(
    entry: Option<PathBuf>,
    cwd_file: Option<PathBuf>,
    chooser_file: Option<PathBuf>,
) -> Result<()> {
    remote::export_env();
    // Three independent start-up jobs run at once: decoding the (cached)
    // config, reading the starting directory, and setting up the terminal.
    let config = std::thread::spawn(config::load_all);
    let (start, _) = app::App::resolve_start(entry.as_deref());
    let listing = {
        let start = start.clone();
        std::thread::spawn(move || {
            let r = fs::read_dir(&start);
            warm_up();
            (start, r)
        })
    };

    let main_thread = std::thread::current().id();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Only a panic on the UI thread takes the screen down with it.
        if std::thread::current().id() == main_thread {
            app::restore_terminal(true);
        }
        hook(info);
    }));
    crossterm::terminal::enable_raw_mode()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::BufWriter::with_capacity(
        1 << 16,
        std::io::stdout(),
    ));
    let mut terminal = ratatui::Terminal::new(backend)?;
    trace("terminal");

    let loaded = config.join().unwrap_or_else(|_| config::load_all());
    let mut app = app::App::with_config(
        app::Options {
            entry,
            cwd_file,
            chooser_file,
        },
        loaded,
    );
    trace("app built");
    app::init_terminal(app.cfg.mgr.mouse);
    trace("term init");
    let prefetched = listing.join().ok();
    trace("listing joined");
    let r = app.run_loop(&mut terminal, prefetched);
    app::restore_terminal(app.cfg.mgr.mouse);
    app.finish();
    r
}

/// Run the text-drawing paths once, off the main thread, so their code is
/// already paged in when the first frame needs it.
fn warm_up() {
    use ratatui::layout::Rect;
    use ratatui::widgets::{Paragraph, Widget};
    let area = Rect::new(0, 0, 120, 4);
    let mut buf = ratatui::buffer::Buffer::empty(area);
    Paragraph::new("warm up ✓ │ 1.2K").render(area, &mut buf);
    buf.set_stringn(0, 1, "  warm", 10, ratatui::style::Style::default());
    std::hint::black_box(&buf);
}

fn debug_info() -> Result<()> {
    println!("wisp {}", env!("CARGO_PKG_VERSION"));
    println!("\nConfig search path (first match per file wins):");
    for d in config::config_dirs() {
        println!("  {}", d.display());
    }
    for f in ["yazi.toml", "keymap.toml", "theme.toml"] {
        match config::find_user_file(f) {
            Some(p) => println!("  {f:<12} {}", p.display()),
            None => println!("  {f:<12} (built-in defaults)"),
        }
    }
    let cfg = config::Config::load();
    let th = theme::Theme::load();
    let km = keymap::Keymap::load();
    println!(
        "\nRatio {:?}, sort {:?}, linemode {}",
        cfg.mgr.ratio, cfg.mgr.sort, cfg.mgr.linemode
    );
    println!(
        "{} manager key bindings",
        km.bindings(keymap::Layer::Mgr).len()
    );
    for w in cfg.warnings.iter().chain(&th.warnings).chain(&km.warnings) {
        println!("warning: {w}");
    }
    println!("\nCache: {}", config::cache_dir().display());
    for t in [
        "fzf",
        "zoxide",
        "ffmpegthumbnailer",
        "pdftoppm",
        "magick",
        "bsdtar",
        "file",
        "wl-copy",
        "xclip",
    ] {
        println!(
            "  {t:<18} {}",
            if util::on_path(t) { "found" } else { "missing" }
        );
    }
    Ok(())
}

fn default_root() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine your home directory; pass a path explicitly")
}

fn cmd_index(
    cache: Option<&std::path::Path>,
    path: Option<PathBuf>,
    incremental: bool,
) -> Result<()> {
    let root = match path {
        Some(p) => p,
        None => default_root()?,
    };
    let opts = IndexOptions::default();
    let mut ds = if incremental {
        DeepSearch::open_or_empty(cache)?
    } else {
        DeepSearch::empty()
    };
    println!(
        "{} {}",
        if incremental {
            "Updating index for"
        } else {
            "Indexing"
        },
        root.display()
    );
    let stats = with_progress("Indexing", |progress| {
        if incremental {
            ds.update(&root, &opts, progress)
        } else {
            ds.build(&root, &opts, progress)
        }
    });
    if ds.maybe_compact(0.3) {
        println!("Compacted index (reclaimed tombstoned documents).");
    }
    ds.save(cache).context("saving index")?;
    println!(
        "Done: {} indexed, {} unchanged, {} removed, {} errors. {} live documents.",
        stats.indexed,
        stats.unchanged,
        stats.removed,
        stats.errors,
        ds.len()
    );
    Ok(())
}

fn cmd_query(cache: Option<&std::path::Path>, query: &str, limit: usize, json: bool) -> Result<()> {
    let ds = load_or_hint(cache)?;
    let opts = QueryOptions {
        limit,
        ..QueryOptions::default()
    };
    let results = ds.search(query, &opts);
    if json {
        let v: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                serde_json::json!({
                    "score": r.score,
                    "type": r.file_type.as_str(),
                    "size": r.size,
                    "path": r.path.display().to_string(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else if results.is_empty() {
        println!("No results for {query:?}.");
    } else {
        for (i, r) in results.iter().enumerate() {
            println!(
                "{:>2}. {:>7.3}  [{}]  {}",
                i + 1,
                r.score,
                r.file_type.as_str(),
                r.path.display()
            );
        }
    }
    Ok(())
}

fn cmd_stats(cache: Option<&std::path::Path>) -> Result<()> {
    let ds = load_or_hint(cache)?;
    let idx = ds.index();
    println!("Live documents:     {}", idx.live_docs);
    println!("Doc slots (w/ dead):{}", idx.docs.len());
    println!("Content terms:      {}", idx.content_index.len());
    println!("Filename terms:     {}", idx.name_index.len());
    println!("Avg content length: {:.1} tokens", idx.avg_content_len());
    println!("Tombstone ratio:    {:.1}%", idx.dead_ratio() * 100.0);
    Ok(())
}

fn load_or_hint(cache: Option<&std::path::Path>) -> Result<DeepSearch> {
    match DeepSearch::load(cache) {
        Ok(ds) => Ok(ds),
        Err(wisp_core::DeepSearchError::NotFound(_)) => {
            anyhow::bail!("no index found. Run `wisp index` first.")
        }
        Err(e) => Err(e.into()),
    }
}

fn with_progress<F, R>(label: &str, f: F) -> R
where
    F: FnOnce(&Progress) -> R,
{
    let progress = Arc::new(Progress::default());
    let done_flag = Arc::new(AtomicBool::new(false));
    let p = progress.clone();
    let flag = done_flag.clone();
    let label_owned = label.to_string();
    let printer = std::thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            let (done, total) = p.snapshot();
            if total > 0 {
                let pct = (done as f64 / total as f64 * 100.0).min(100.0);
                eprint!("\r{label_owned}: {done}/{total} ({pct:.0}%)      ");
            } else {
                eprint!("\r{label_owned}: scanning...      ");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let result = f(&progress);
    done_flag.store(true, Ordering::Relaxed);
    let _ = printer.join();
    let (done, total) = progress.snapshot();
    eprintln!("\r{label}: {done}/{total} files scanned.            ");
    result
}

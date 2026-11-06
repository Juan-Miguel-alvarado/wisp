//! `wisp emit` — send commands to a running instance, like `ya emit`.
//!
//! Every instance listens on `$XDG_RUNTIME_DIR/wisp/<pid>.sock` and
//! exports `WISP_ID` / `YAZI_ID` (and bumps `YAZI_LEVEL`) so shells it
//! starts know which instance they belong to.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use anyhow::{bail, Context, Result};

use crate::event::Msg;

fn dir() -> PathBuf {
    dirs::runtime_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wisp")
}

fn sock(id: u32) -> PathBuf {
    dir().join(format!("{id}.sock"))
}

/// Export the instance id and nesting level to child processes.
pub fn export_env() {
    let id = std::process::id().to_string();
    let level: u32 = std::env::var("YAZI_LEVEL")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(0)
        + 1;
    // SAFETY: called on the main thread before any other thread reads env.
    unsafe {
        std::env::set_var("WISP_ID", &id);
        std::env::set_var("YAZI_ID", &id);
        std::env::set_var("YAZI_LEVEL", level.to_string());
    }
}

/// Start listening for remote commands.
pub fn listen(tx: Sender<Msg>) {
    let d = dir();
    let _ = std::fs::create_dir_all(&d);
    // Sockets left by instances that were killed (no clean exit).
    for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
        let pid = e
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<i32>().ok());
        // SAFETY: kill with signal 0 only checks whether the process exists.
        if let Some(pid) = pid {
            if unsafe { libc::kill(pid, 0) } != 0 {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = sock(std::process::id());
    let _ = std::fs::remove_file(&path);
    let Ok(l) = UnixListener::bind(&path) else {
        return;
    };
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(s).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() && tx.send(Msg::Remote(line)).is_err() {
                        return;
                    }
                }
            });
        }
    });
}

pub fn cleanup() {
    let _ = std::fs::remove_file(sock(std::process::id()));
}

fn send(id: u32, line: &str) -> Result<()> {
    let mut s = UnixStream::connect(sock(id))
        .with_context(|| format!("no running instance with id {id}"))?;
    writeln!(s, "{line}")?;
    Ok(())
}

/// `wisp emit <cmd> [args…]`: to `$WISP_ID`/`$YAZI_ID`, else to
/// every running instance. `emit-to <id> <cmd>` targets one.
pub fn emit(args: &[String], to: Option<u32>) -> Result<()> {
    if args.is_empty() {
        bail!("usage: wisp emit <command> [args…]");
    }
    let line = args
        .iter()
        .map(|a| crate::util::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    let env_id = std::env::var("WISP_ID")
        .or_else(|_| std::env::var("YAZI_ID"))
        .ok()
        .and_then(|v| v.parse().ok());
    if let Some(id) = to.or(env_id) {
        return send(id, &line);
    }
    let mut n = 0;
    for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
        let Some(id) = e
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if send(id, &line).is_ok() {
            n += 1;
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    if n == 0 {
        bail!("no running wisp instance");
    }
    Ok(())
}

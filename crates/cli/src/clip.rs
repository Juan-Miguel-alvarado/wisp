//! Copy text to the system clipboard by shelling out to whatever clipboard
//! tool is installed. Avoids pulling in a clipboard crate (and an X/Wayland
//! build dependency) for a one-shot copy.
//!
//! Detection order matches the common setups: Wayland (`wl-copy`), X11
//! (`xclip`/`xsel`), macOS (`pbcopy`), and Windows (`clip`).

use std::io::Write;
use std::process::{Command, Stdio};

/// A clipboard tool: the program plus any fixed arguments.
struct Tool {
    program: &'static str,
    args: &'static [&'static str],
}

const TOOLS: &[Tool] = &[
    Tool {
        program: "wl-copy",
        args: &[],
    },
    Tool {
        program: "xclip",
        args: &["-selection", "clipboard"],
    },
    Tool {
        program: "xsel",
        args: &["--clipboard", "--input"],
    },
    Tool {
        program: "pbcopy",
        args: &[],
    },
    Tool {
        program: "clip",
        args: &[],
    },
];

/// Copy `text` to the clipboard. Returns the tool used on success, or an error
/// message describing why it couldn't.
pub fn copy(text: &str) -> Result<&'static str, String> {
    // Over SSH the useful clipboard is the local one: ask the terminal.
    let remote = std::env::var_os("SSH_TTY").is_some();
    let tool = TOOLS.iter().find(|t| on_path(t.program));
    let Some(tool) = tool.filter(|_| !remote) else {
        return osc52(text);
    };

    let mut child = Command::new(tool.program)
        .args(tool.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to run {}: {e}", tool.program))?;

    if let Some(stdin) = child.stdin.take() {
        // Drop the handle after writing so the tool sees EOF and exits.
        let mut stdin = stdin;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write to {}: {e}", tool.program))?;
    }

    match child.wait() {
        Ok(status) if status.success() => Ok(tool.program),
        Ok(status) => Err(format!("{} exited with {status}", tool.program)),
        Err(e) => Err(format!("{} failed: {e}", tool.program)),
    }
}

/// Set the clipboard through the terminal itself (OSC 52), which works in
/// most modern terminals and across SSH.
fn osc52(text: &str) -> Result<&'static str, String> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = text.as_bytes();
    let mut enc = String::with_capacity(b.len().div_ceil(3) * 4);
    for chunk in b.chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            _ => (chunk[0] as u32) << 16,
        };
        for i in 0..4 {
            if i <= chunk.len() {
                enc.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                enc.push('=');
            }
        }
    }
    let mut out = std::io::stdout();
    out.write_all(format!("\x1b]52;c;{enc}\x07").as_bytes())
        .and_then(|_| out.flush())
        .map_err(|e| e.to_string())?;
    Ok("osc52")
}

fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file())
}

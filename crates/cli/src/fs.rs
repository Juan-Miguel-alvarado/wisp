//! Files, their characteristics, MIME detection, and fast directory reading.
//!
//! Reading is where a file manager spends its time on big directories, so it
//! is done in one pass: `readdir` for names, then `statx` per entry fanned out
//! across cores (rayon) once a directory is large enough to benefit. MIME
//! types come from the extension (a table lookup, no process spawn); content
//! sniffing is reserved for files without a known extension, and done lazily.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::{self, DirEntry, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use rayon::prelude::*;

use crate::config::{SortBy, SortOpts};
use crate::theme::flags;
use crate::util::{alpha_cmp, natural_cmp};

/// File characteristics (a compact `stat`).
#[derive(Clone, Debug, Default)]
pub struct Cha {
    pub flags: u16,
    pub len: u64,
    pub mtime: i64,
    pub mtime_ns: u32,
    pub btime: i64,
    pub atime: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
    pub ino: u64,
}

impl Cha {
    #[allow(clippy::unnecessary_cast)] // `S_ISVTX` is `u16` on macOS
    pub fn from_meta(m: &Metadata) -> Self {
        let mode = m.mode();
        let ft = m.file_type();
        let mut f = 0;
        if ft.is_dir() {
            f |= flags::DIR;
        }
        use std::os::unix::fs::FileTypeExt;
        if ft.is_block_device() {
            f |= flags::BLOCK;
        }
        if ft.is_char_device() {
            f |= flags::CHAR;
        }
        if ft.is_fifo() {
            f |= flags::FIFO;
        }
        if ft.is_socket() {
            f |= flags::SOCK;
        }
        if !ft.is_dir() && mode & 0o111 != 0 {
            f |= flags::EXEC;
        }
        if mode & (libc::S_ISVTX as u32) != 0 {
            f |= flags::STICKY;
        }
        let btime = m
            .created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self {
            flags: f,
            len: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec() as u32,
            btime,
            atime: m.atime(),
            mode,
            uid: m.uid(),
            gid: m.gid(),
            nlink: m.nlink(),
            ino: m.ino(),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    pub fn is_orphan(&self) -> bool {
        self.flags & flags::ORPHAN != 0
    }
}

/// One directory entry.
#[derive(Clone, Debug)]
pub struct File {
    pub url: PathBuf,
    /// Display name (lossy UTF-8).
    pub name: Box<str>,
    pub cha: Cha,
    /// Symlink target, as stored in the link.
    pub link_to: Option<PathBuf>,
}

impl File {
    /// Stat `path` (not following a final symlink first, like `lstat`).
    pub fn from_path(path: &Path) -> io::Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        Ok(Self::from_meta(path.to_path_buf(), meta))
    }

    fn from_meta(url: PathBuf, meta: Metadata) -> Self {
        let name: Box<str> = url
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_else(|| url.to_string_lossy().into());
        let mut link_to = None;
        let cha = if meta.file_type().is_symlink() {
            link_to = fs::read_link(&url).ok();
            match fs::metadata(&url) {
                Ok(t) => {
                    let mut c = Cha::from_meta(&t);
                    c.flags |= flags::LINK;
                    c
                }
                Err(_) => {
                    let mut c = Cha::from_meta(&meta);
                    c.flags |= flags::LINK | flags::ORPHAN;
                    c
                }
            }
        } else {
            Cha::from_meta(&meta)
        };
        let mut f = Self {
            url,
            name,
            cha,
            link_to,
        };
        if f.name.starts_with('.') {
            f.cha.flags |= flags::HIDDEN;
        }
        f
    }

    fn from_entry(e: &DirEntry) -> Self {
        let url = e.path();
        match e.metadata() {
            Ok(meta) => Self::from_meta(url, meta),
            Err(_) => {
                let name: Box<str> = e.file_name().to_string_lossy().into();
                let mut cha = Cha {
                    flags: flags::DUMMY,
                    ..Default::default()
                };
                if name.starts_with('.') {
                    cha.flags |= flags::HIDDEN;
                }
                Self {
                    url,
                    name,
                    cha,
                    link_to: None,
                }
            }
        }
    }

    pub fn is_dir(&self) -> bool {
        self.cha.is_dir()
    }

    pub fn is_hidden(&self) -> bool {
        self.cha.flags & flags::HIDDEN != 0
    }

    /// Extension without the dot, if any (`.bashrc` has none).
    pub fn ext(&self) -> Option<&str> {
        let n = self.name.as_ref();
        let i = n.rfind('.')?;
        (i > 0 && i + 1 < n.len()).then(|| &n[i + 1..])
    }

    /// The MIME type known without touching the file's contents.
    pub fn quick_mime(&self) -> Option<&'static str> {
        if self.is_dir() {
            return Some("inode/directory");
        }
        if self.cha.flags & flags::DUMMY != 0 {
            return Some("null/dummy");
        }
        if self.cha.flags & (flags::BLOCK | flags::CHAR | flags::FIFO | flags::SOCK) != 0 {
            return Some("inode/special");
        }
        if self.cha.len == 0 {
            return Some("inode/empty");
        }
        mime_by_name(&self.name)
    }
}

/// Normalize a MIME type the way yazi does: drop the `x-` vendor prefix of
/// the subtype, so rules like `application/7z*` match `x-7z-compressed`.
fn normalize(m: &'static str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let Some((ty, sub)) = m.split_once('/') else {
        return m;
    };
    let Some(rest) = sub.strip_prefix("x-") else {
        return m;
    };
    let mut c = CACHE.get_or_init(Default::default).lock().unwrap();
    c.entry(m)
        .or_insert_with(|| Box::leak(format!("{ty}/{rest}").into_boxed_str()))
}

/// MIME type by file name/extension.
pub fn mime_by_name(name: &str) -> Option<&'static str> {
    let lower;
    let ext = match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => {
            let e = &name[i + 1..];
            if e.bytes().any(|b| b.is_ascii_uppercase()) {
                lower = e.to_ascii_lowercase();
                lower.as_str()
            } else {
                e
            }
        }
        _ => {
            return match name {
                "Makefile" | "makefile" | "Dockerfile" | "LICENSE" | "README" | "COPYING"
                | "PKGBUILD" | "Justfile" | "justfile" | "Containerfile" | "CODEOWNERS" => {
                    Some("text/plain")
                }
                _ => None,
            }
        }
    };
    // Extensions mime_guess does not know (or gets wrong for our purposes).
    let fixed = match ext {
        "rs" => "text/rust",
        "toml" | "lock" | "ini" | "cfg" | "conf" | "env" | "properties" => "text/plain",
        "lua" => "text/lua",
        "go" => "text/go",
        "nix" | "zig" | "kt" | "kts" | "swift" | "dart" | "ex" | "exs" | "erl" | "hs" | "ml"
        | "mli" | "clj" | "scala" | "sql" | "vim" | "fish" | "zsh" | "bash" | "ksh" | "ps1"
        | "gradle" | "cmake" | "tf" | "hcl" | "proto" | "graphql" | "gql" | "vue" | "svelte"
        | "astro" | "tsx" | "jsx" | "ts" | "mts" | "cts" | "rb" | "php" | "pl" | "r" | "jl"
        | "nim" | "v" | "sv" | "vhd" | "asm" | "s" | "diff" | "patch" | "log" | "gitignore"
        | "editorconfig" | "csv" | "tsv" | "rst" | "adoc" | "org" | "tex" | "bib" | "srt"
        | "vtt" | "desktop" | "service" | "timer" | "rules" | "kdl" | "ron" | "jsonc" | "json5"
        | "yml" | "yaml" | "mdx" | "qml" | "glsl" | "hlsl" | "wgsl" => "text/plain",
        "md" | "markdown" => "text/markdown",
        "json" => "application/json",
        "ndjson" | "jsonl" => "application/ndjson",
        "js" | "mjs" | "cjs" => "application/javascript",
        "7z" => "application/7z-compressed",
        "zst" => "application/zstd",
        "xz" => "application/xz",
        "rar" => "application/rar",
        "tgz" => "application/gzip",
        "deb" => "application/debian-package",
        "rpm" => "application/rpm",
        "apk" => "application/android.package-archive",
        "iso" => "application/iso9660-image",
        "qcow2" => "application/qemu-disk",
        "dmg" => "application/apple-diskimage",
        "heic" | "heif" => "image/heic",
        "avif" => "image/avif",
        "jxl" => "image/jxl",
        "webp" => "image/webp",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "opus" => "audio/opus",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "",
    };
    if !fixed.is_empty() {
        return Some(normalize(fixed));
    }
    mime_guess::from_ext(ext).first_raw().map(normalize)
}

/// Sniff a file's MIME type from its first bytes. Used for files whose
/// extension says nothing.
pub fn sniff_mime(path: &Path) -> String {
    let mut buf = [0u8; 1024];
    let n = match fs::File::open(path).and_then(|mut f| f.read(&mut buf)) {
        Ok(n) => n,
        Err(_) => return "null/unreadable".into(),
    };
    let head = &buf[..n];
    if n == 0 {
        return "inode/empty".into();
    }
    if let Some(t) = infer::get(head) {
        return normalize(t.mime_type()).to_string();
    }
    if head.starts_with(b"#!") || content_inspector::inspect(head).is_text() {
        return "text/plain".into();
    }
    "application/octet-stream".into()
}

/// The full MIME of a file: by name, falling back to content sniffing.
pub fn mime_of(file: &File) -> String {
    match file.quick_mime() {
        Some(m) => m.to_string(),
        None => sniff_mime(&file.url),
    }
}

/// Read all entries of `dir`. Large directories are stat'ed in parallel.
pub fn read_dir(dir: &Path) -> io::Result<Vec<File>> {
    let entries: Vec<DirEntry> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    let files = if entries.len() >= 1024 {
        entries.par_iter().map(File::from_entry).collect()
    } else {
        entries.iter().map(File::from_entry).collect()
    };
    Ok(files)
}

fn ext_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}

/// Sort files in place according to `opts`.
pub fn sort_files(files: &mut [File], opts: SortOpts) {
    let s = opts.sensitive;
    let name_cmp = move |a: &File, b: &File| -> Ordering {
        match opts.by {
            SortBy::Natural => natural_cmp(&a.name, &b.name, s),
            _ => alpha_cmp(&a.name, &b.name, s),
        }
    };
    if opts.by == SortBy::Random {
        fastrand::shuffle(files);
        if opts.dir_first {
            files.sort_by_key(|f| !f.is_dir());
        }
        return;
    }
    let cmp = move |a: &File, b: &File| -> Ordering {
        if opts.dir_first {
            match (a.is_dir(), b.is_dir()) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
        }
        let o = match opts.by {
            SortBy::None => Ordering::Equal,
            SortBy::Alphabetical | SortBy::Natural => name_cmp(a, b),
            SortBy::Mtime => a
                .cha
                .mtime
                .cmp(&b.cha.mtime)
                .then(a.cha.mtime_ns.cmp(&b.cha.mtime_ns))
                .then_with(|| name_cmp(a, b)),
            SortBy::Btime => a.cha.btime.cmp(&b.cha.btime).then_with(|| name_cmp(a, b)),
            SortBy::Size => a.cha.len.cmp(&b.cha.len).then_with(|| name_cmp(a, b)),
            SortBy::Extension => {
                let (ea, eb) = (ext_of(&a.name), ext_of(&b.name));
                alpha_cmp(ea, eb, s).then_with(|| name_cmp(a, b))
            }
            SortBy::Random => Ordering::Equal,
        };
        if opts.reverse {
            o.reverse()
        } else {
            o
        }
    };
    if opts.by == SortBy::None && !opts.dir_first {
        return;
    }
    if files.len() > 4096 {
        files.par_sort_by(cmp);
    } else {
        files.sort_by(cmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mimes() {
        assert_eq!(mime_by_name("a.png"), Some("image/png"));
        assert_eq!(mime_by_name("a.7z"), Some("application/7z-compressed"));
        assert_eq!(mime_by_name("a.tar"), Some("application/tar"));
        assert_eq!(mime_by_name("a.gz"), Some("application/gzip"));
        assert_eq!(mime_by_name("a.zip"), Some("application/zip"));
        assert_eq!(mime_by_name("a.JPG"), Some("image/jpeg"));
        assert_eq!(mime_by_name(".bashrc"), None);
        assert!(mime_by_name("a.mp4").unwrap().starts_with("video/"));
        assert_eq!(mime_by_name("a.pdf"), Some("application/pdf"));
    }

    #[test]
    fn reads_and_sorts() {
        let d = tempfile::tempdir().unwrap();
        for n in ["b10", "b2", "A1", ".hidden"] {
            std::fs::write(d.path().join(n), "x").unwrap();
        }
        std::fs::create_dir(d.path().join("zdir")).unwrap();
        let mut files = read_dir(d.path()).unwrap();
        sort_files(
            &mut files,
            SortOpts {
                by: SortBy::Natural,
                sensitive: false,
                reverse: false,
                dir_first: true,
            },
        );
        let names: Vec<&str> = files.iter().map(|f| &*f.name).collect();
        assert_eq!(names, vec!["zdir", ".hidden", "A1", "b2", "b10"]);
        assert!(files[1].is_hidden());
    }
}

//! Small helpers shared across the file manager: human sizes, local time
//! formatting, permission strings, user/group names, natural sorting and
//! cell-width aware truncation. No extra dependencies beyond `libc`.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Format a byte count the way yazi does: `6B`, `1.5K`, `12.0M`.
pub fn readable_size(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B", "K", "M", "G", "T", "P", "E"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    let mut n = format!("{size:.1}");
    if n.ends_with(".0") {
        n.truncate(n.len() - 2);
    }
    format!("{n}{}", UNITS[unit])
}

/// Same as [`readable_size`] but with a space and `B` suffix, for prose.
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Broken-down local time.
pub struct Tm {
    pub year: i32,
    pub mon: u32,
    pub day: u32,
    pub hour: u32,
    pub min: u32,
    pub sec: u32,
}

/// Convert epoch seconds to local time via `localtime_r`.
pub fn local_time(secs: i64) -> Tm {
    // SAFETY: `localtime_r` only writes into the provided struct.
    unsafe {
        let t: libc::time_t = secs as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        Tm {
            year: tm.tm_year + 1900,
            mon: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
            hour: tm.tm_hour as u32,
            min: tm.tm_min as u32,
            sec: tm.tm_sec as u32,
        }
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// yazi's linemode time format: `%m/%d %H:%M` this year, `%m/%d  %Y` otherwise.
pub fn linemode_time(secs: i64) -> String {
    if secs == 0 {
        return String::new();
    }
    let t = local_time(secs);
    let now = local_time(now_secs());
    if t.year == now.year {
        format!("{:02}/{:02} {:02}:{:02}", t.mon, t.day, t.hour, t.min)
    } else {
        format!("{:02}/{:02}  {}", t.mon, t.day, t.year)
    }
}

/// Full `YYYY-MM-DD HH:MM:SS` local timestamp.
pub fn full_time(secs: i64) -> String {
    if secs == 0 {
        return "-".into();
    }
    let t = local_time(secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.mon, t.day, t.hour, t.min, t.sec
    )
}

/// `ls -l`-style permission string, e.g. `drwxr-xr-x`.
// The casts are no-ops on Linux but needed where `mode_t` is `u16` (macOS).
#[allow(clippy::unnecessary_cast)]
pub fn perm_string(mode: u32) -> String {
    const FMT: u32 = libc::S_IFMT as u32;
    const DIR: u32 = libc::S_IFDIR as u32;
    const LNK: u32 = libc::S_IFLNK as u32;
    const CHR: u32 = libc::S_IFCHR as u32;
    const BLK: u32 = libc::S_IFBLK as u32;
    const FIFO: u32 = libc::S_IFIFO as u32;
    const SOCK: u32 = libc::S_IFSOCK as u32;
    let kind = match mode & FMT {
        DIR => 'd',
        LNK => 'l',
        CHR => 'c',
        BLK => 'b',
        FIFO => 'p',
        SOCK => 's',
        _ => '-',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let bit = |m: u32, c: char| if mode & m != 0 { c } else { '-' };
    s.push(bit(libc::S_IRUSR as u32, 'r'));
    s.push(bit(libc::S_IWUSR as u32, 'w'));
    s.push(
        match (
            mode & (libc::S_IXUSR as u32) != 0,
            mode & (libc::S_ISUID as u32) != 0,
        ) {
            (true, true) => 's',
            (false, true) => 'S',
            (true, false) => 'x',
            _ => '-',
        },
    );
    s.push(bit(libc::S_IRGRP as u32, 'r'));
    s.push(bit(libc::S_IWGRP as u32, 'w'));
    s.push(
        match (
            mode & (libc::S_IXGRP as u32) != 0,
            mode & (libc::S_ISGID as u32) != 0,
        ) {
            (true, true) => 's',
            (false, true) => 'S',
            (true, false) => 'x',
            _ => '-',
        },
    );
    s.push(bit(libc::S_IROTH as u32, 'r'));
    s.push(bit(libc::S_IWOTH as u32, 'w'));
    s.push(
        match (
            mode & (libc::S_IXOTH as u32) != 0,
            mode & (libc::S_ISVTX as u32) != 0,
        ) {
            (true, true) => 't',
            (false, true) => 'T',
            (true, false) => 'x',
            _ => '-',
        },
    );
    s
}

/// uid → user name and gid → group name.
type NameCache = Mutex<(HashMap<u32, String>, HashMap<u32, String>)>;

fn name_cache() -> &'static NameCache {
    static CACHE: OnceLock<NameCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new((HashMap::new(), HashMap::new())))
}

/// Resolve a uid to a user name (cached); falls back to the number.
pub fn user_name(uid: u32) -> String {
    let mut c = name_cache().lock().unwrap();
    if let Some(n) = c.0.get(&uid) {
        return n.clone();
    }
    // SAFETY: getpwuid_r writes into our buffers only.
    let name = unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 1024];
        let mut res: *mut libc::passwd = std::ptr::null_mut();
        if libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut res) == 0
            && !res.is_null()
        {
            std::ffi::CStr::from_ptr(pwd.pw_name)
                .to_string_lossy()
                .into_owned()
        } else {
            uid.to_string()
        }
    };
    c.0.insert(uid, name.clone());
    name
}

/// Resolve a gid to a group name (cached); falls back to the number.
pub fn group_name(gid: u32) -> String {
    let mut c = name_cache().lock().unwrap();
    if let Some(n) = c.1.get(&gid) {
        return n.clone();
    }
    // SAFETY: getgrgid_r writes into our buffers only.
    let name = unsafe {
        let mut grp: libc::group = std::mem::zeroed();
        let mut buf = vec![0 as libc::c_char; 4096];
        let mut res: *mut libc::group = std::ptr::null_mut();
        if libc::getgrgid_r(gid, &mut grp, buf.as_mut_ptr(), buf.len(), &mut res) == 0
            && !res.is_null()
        {
            std::ffi::CStr::from_ptr(grp.gr_name)
                .to_string_lossy()
                .into_owned()
        } else {
            gid.to_string()
        }
    };
    c.1.insert(gid, name.clone());
    name
}

/// The user's home directory, cached.
pub fn home() -> &'static Option<PathBuf> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(dirs::home_dir)
}

/// Render a path for display, collapsing the home directory to `~`.
pub fn readable_path(path: &Path) -> String {
    if let Some(home) = home() {
        if let Ok(rest) = path.strip_prefix(home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rest.to_string_lossy());
        }
    }
    path.to_string_lossy().into_owned()
}

/// Expand a leading `~` and environment variables (`$VAR`, `${VAR}`).
pub fn expand_path(s: &str) -> PathBuf {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            let braced = chars.peek() == Some(&'{');
            if braced {
                chars.next();
            }
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if braced {
                    chars.next();
                    if n == '}' {
                        break;
                    }
                    name.push(n);
                } else if n.is_ascii_alphanumeric() || n == '_' {
                    name.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            if name.is_empty() {
                out.push('$');
            } else {
                out.push_str(&std::env::var(&name).unwrap_or_default());
            }
        } else {
            out.push(c);
        }
    }
    if out == "~" {
        if let Some(h) = home() {
            return h.clone();
        }
    }
    if let Some(rest) = out.strip_prefix("~/") {
        if let Some(h) = home() {
            return h.join(rest);
        }
    }
    PathBuf::from(out)
}

/// Lexically normalize `.` and `..` components without touching the disk.
pub fn clean_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Natural ("human") comparison: digit runs compare numerically, so
/// `file2` < `file10`. Optionally case-insensitive.
pub fn natural_cmp(a: &str, b: &str, sensitive: bool) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (ca, cb) = (a[i], b[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            // Skip leading zeros, then compare run length, then digits.
            let si = i;
            let sj = j;
            while i < a.len() && a[i] == b'0' {
                i += 1;
            }
            while j < b.len() && b[j] == b'0' {
                j += 1;
            }
            let ni = i;
            let nj = j;
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let (la, lb) = (i - ni, j - nj);
            if la != lb {
                return la.cmp(&lb);
            }
            match a[ni..i].cmp(&b[nj..j]) {
                Ordering::Equal => {}
                o => return o,
            }
            // Same value: fewer leading zeros first.
            match (ni - si).cmp(&(nj - sj)) {
                Ordering::Equal => {}
                o => return o,
            }
            continue;
        }
        let (x, y) = if sensitive {
            (ca, cb)
        } else {
            (ca.to_ascii_lowercase(), cb.to_ascii_lowercase())
        };
        if x != y {
            return x.cmp(&y);
        }
        i += 1;
        j += 1;
    }
    (a.len() - i).cmp(&(b.len() - j))
}

/// Plain (optionally case-insensitive) byte-wise comparison.
pub fn alpha_cmp(a: &str, b: &str, sensitive: bool) -> Ordering {
    if sensitive {
        return a.cmp(b);
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (x.to_ascii_lowercase(), y.to_ascii_lowercase());
        if x != y {
            return x.cmp(&y);
        }
    }
    a.len().cmp(&b.len())
}

/// Terminal width of a string, in cells.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` cells (keeping the start).
pub fn truncate(s: &str, max: usize) -> &str {
    let mut w = 0;
    for (i, c) in s.char_indices() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            return &s[..i];
        }
        w += cw;
    }
    s
}

/// Truncate keeping the *end* of the string, prefixing `…` when cut.
pub fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut w = 1; // the ellipsis
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max {
            break;
        }
        w += cw;
        start = i;
    }
    format!("…{}", &s[start..])
}

/// Replace control characters so a hostile filename cannot corrupt the screen.
pub fn printable(s: &str) -> std::borrow::Cow<'_, str> {
    if s.chars().all(|c| !c.is_control()) {
        return std::borrow::Cow::Borrowed(s);
    }
    std::borrow::Cow::Owned(
        s.chars()
            .map(|c| if c.is_control() { '?' } else { c })
            .collect(),
    )
}

/// Whether `bin` is an executable on `$PATH` (cached).
pub fn on_path(bin: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&v) = cache.lock().unwrap().get(bin) {
        return v;
    }
    let found = std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false);
    cache.lock().unwrap().insert(bin.to_string(), found);
    found
}

/// Quote a string for POSIX `sh`.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Split a command line into words, honouring single/double quotes and
/// backslash escapes (enough for yazi-style keymap `run` strings).
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                has = true;
                for n in chars.by_ref() {
                    if n == '\'' {
                        break;
                    }
                    cur.push(n);
                }
            }
            '"' => {
                has = true;
                while let Some(n) = chars.next() {
                    match n {
                        '"' => break,
                        '\\' => {
                            if let Some(e) = chars.next() {
                                cur.push(e);
                            }
                        }
                        _ => cur.push(n),
                    }
                }
            }
            '\\' => {
                has = true;
                if let Some(e) = chars.next() {
                    cur.push(e);
                }
            }
            c if c.is_whitespace() => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                has = true;
                cur.push(c);
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// A unique, non-existing sibling of `path`: `name_1.ext`, `name_2.ext`, …
pub fn unique_name(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_err() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for i in 1.. {
        let cand = parent.join(format!("{stem}_{i}{ext}"));
        if std::fs::symlink_metadata(&cand).is_err() {
            return cand;
        }
    }
    unreachable!()
}

/// Compute a relative path from `base` (a directory) to `target`.
pub fn relative_path(base: &Path, target: &Path) -> PathBuf {
    use std::path::Component;
    let b: Vec<Component> = base.components().collect();
    let t: Vec<Component> = target.components().collect();
    let common = b.iter().zip(&t).take_while(|(x, y)| x == y).count();
    let mut out = PathBuf::new();
    for _ in common..b.len() {
        out.push("..");
    }
    for c in &t[common..] {
        out.push(c.as_os_str());
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(readable_size(6), "6B");
        assert_eq!(readable_size(1536), "1.5K");
        assert_eq!(readable_size(8192), "8K");
        assert_eq!(human_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["f10", "f2", "F1", "f02", "a"];
        v.sort_by(|a, b| natural_cmp(a, b, false));
        assert_eq!(v, vec!["a", "F1", "f2", "f02", "f10"]);
    }

    #[test]
    fn perms() {
        assert_eq!(perm_string(0o40755), "drwxr-xr-x");
        assert_eq!(perm_string(0o100644), "-rw-r--r--");
        assert_eq!(perm_string(0o104755), "-rwsr-xr-x");
    }

    #[test]
    fn words() {
        assert_eq!(
            split_words(r#"shell 'echo "hi" there' --block"#),
            vec!["shell", "echo \"hi\" there", "--block"]
        );
        assert_eq!(split_words("a  b\\ c"), vec!["a", "b c"]);
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_left("/very/long/path", 6), "…/path");
        assert_eq!(truncate("abcdef", 3), "abc");
    }

    #[test]
    fn relative() {
        assert_eq!(
            relative_path(Path::new("/a/b/c"), Path::new("/a/d/e")),
            PathBuf::from("../../d/e")
        );
    }
}

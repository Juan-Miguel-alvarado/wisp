//! Configuration, in yazi's own file format.
//!
//! Three files, each layered over a built-in preset (yazi's defaults, shipped in
//! `preset/`): `yazi.toml` (manager/preview/opener settings), `keymap.toml` and
//! `theme.toml`. User files are looked up per file in:
//!
//! 1. `$WISP_CONFIG_HOME`, or `~/.config/wisp/`
//! 2. `$YAZI_CONFIG_HOME`, or `~/.config/yazi/`
//!
//! so an existing yazi setup (theme, keymap, openers) works unchanged.
//!
//! Merging follows yazi: tables merge key by key, arrays are replaced, and
//! `prepend_<key>` / `append_<key>` add items around the preset's `<key>`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::glob::Glob;
use toml::{Table, Value};

pub const PRESET_YAZI: &str = include_str!("../preset/yazi.toml");
pub const PRESET_KEYMAP: &str = include_str!("../preset/keymap.toml");
pub const PRESET_THEME: &str = include_str!("../preset/theme-dark.toml");

/// Directories searched for user config, most specific first.
pub fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(p) = std::env::var_os("WISP_CONFIG_HOME") {
        dirs.push(PathBuf::from(p));
    } else if let Some(c) = dirs::config_dir() {
        dirs.push(c.join("wisp"));
    }
    if let Some(p) = std::env::var_os("YAZI_CONFIG_HOME") {
        dirs.push(PathBuf::from(p));
    } else if let Some(c) = dirs::config_dir() {
        dirs.push(c.join("yazi"));
    }
    dirs
}

/// Find the first existing user file named `name`.
pub fn find_user_file(name: &str) -> Option<PathBuf> {
    config_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Parse `preset`, then merge the user's `name` file over it.
pub fn load_layered(preset: &str, name: &str) -> (Table, Option<String>) {
    let mut base: Table = preset.parse().expect("built-in preset is valid TOML");
    rename_legacy(&mut base);
    let mut warning = None;
    if let Some(path) = find_user_file(name) {
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|s| s.parse::<Table>().map_err(anyhow::Error::from))
        {
            Ok(mut user) => {
                rename_legacy(&mut user);
                merge(&mut base, user);
            }
            Err(e) => warning = Some(format!("{}: {e}", path.display())),
        }
    }
    (base, warning)
}

/// Old yazi releases called the `[mgr]` section `[manager]`.
fn rename_legacy(t: &mut Table) {
    if let Some(v) = t.remove("manager") {
        match t.get_mut("mgr") {
            Some(Value::Table(m)) => {
                if let Value::Table(old) = v {
                    merge(m, old);
                }
            }
            _ => {
                t.insert("mgr".into(), v);
            }
        }
    }
}

/// Deep-merge `user` into `base` with yazi's prepend/append semantics.
pub fn merge(base: &mut Table, user: Table) {
    let mut prepends = Vec::new();
    let mut appends = Vec::new();
    for (k, v) in user {
        if let Some(name) = k.strip_prefix("prepend_") {
            prepends.push((name.to_string(), v));
            continue;
        }
        if let Some(name) = k.strip_prefix("append_") {
            appends.push((name.to_string(), v));
            continue;
        }
        match (base.get_mut(&k), v) {
            (Some(Value::Table(b)), Value::Table(u)) => merge(b, u),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
    for (name, v) in prepends {
        if let Value::Array(mut items) = v {
            if let Some(Value::Array(existing)) = base.remove(&name) {
                items.extend(existing);
            }
            base.insert(name, Value::Array(items));
        }
    }
    for (name, v) in appends {
        if let Value::Array(items) = v {
            let mut existing = match base.remove(&name) {
                Some(Value::Array(a)) => a,
                _ => Vec::new(),
            };
            existing.extend(items);
            base.insert(name, Value::Array(existing));
        }
    }
}

// --------------------------------------------------------------------------
// yazi.toml
// --------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortBy {
    None,
    Mtime,
    Btime,
    Extension,
    Alphabetical,
    Natural,
    Size,
    Random,
}

impl SortBy {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "mtime" | "modified" => Self::Mtime,
            "btime" | "created" => Self::Btime,
            "extension" => Self::Extension,
            "alphabetical" => Self::Alphabetical,
            "natural" => Self::Natural,
            "size" => Self::Size,
            "random" => Self::Random,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SortOpts {
    pub by: SortBy,
    pub sensitive: bool,
    pub reverse: bool,
    pub dir_first: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mgr {
    pub ratio: [u16; 3],
    pub sort: SortOpts,
    pub linemode: String,
    pub show_hidden: bool,
    pub show_symlink: bool,
    pub scrolloff: usize,
    pub mouse: bool,
    pub title_format: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewCfg {
    pub wrap: bool,
    pub tab_size: usize,
    pub max_width: u32,
    pub max_height: u32,
    pub image_quality: u8,
}

/// One way to open a file (`[opener]` entry).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Opener {
    #[serde(alias = "exec")]
    pub run: String,
    #[serde(default)]
    pub block: bool,
    #[serde(default)]
    pub orphan: bool,
    #[serde(default)]
    pub desc: String,
    #[serde(rename = "for", default)]
    pub for_os: Option<String>,
}

impl Opener {
    pub fn applies(&self) -> bool {
        match self.for_os.as_deref() {
            None => true,
            Some("unix") => cfg!(unix),
            Some("linux") => cfg!(target_os = "linux"),
            Some("macos") => cfg!(target_os = "macos"),
            Some("windows") => cfg!(windows),
            Some(_) => false,
        }
    }

    pub fn label(&self) -> String {
        if self.desc.is_empty() {
            self.run.clone()
        } else {
            self.desc.clone()
        }
    }
}

/// A glob matched against a file's full path; a trailing `/` restricts it to
/// directories, otherwise it applies to non-directories (yazi semantics).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UrlPattern {
    glob: Glob,
    dir: bool,
}

impl UrlPattern {
    pub fn new(pat: &str) -> Option<Self> {
        let (pat, dir) = match pat.strip_suffix('/') {
            Some(p) => (p, true),
            None => (pat, false),
        };
        Some(Self {
            glob: Glob::new(pat),
            dir,
        })
    }

    pub fn matches(&self, path: &Path, is_dir: bool) -> bool {
        if self.dir != is_dir {
            return false;
        }
        self.glob.is_match(&path.to_string_lossy())
    }
}

/// A glob over MIME types, e.g. `{audio,video}/*`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MimePattern(Glob);

impl MimePattern {
    pub fn new(pat: &str) -> Option<Self> {
        Some(Self(Glob::new(pat)))
    }

    pub fn matches(&self, mime: &str) -> bool {
        self.0.is_match(mime)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenRule {
    pub url: Option<UrlPattern>,
    pub mime: Option<MimePattern>,
    pub uses: Vec<String>,
}

impl OpenRule {
    pub fn matches(&self, path: &Path, is_dir: bool, mime: &str) -> bool {
        self.url.as_ref().is_none_or(|u| u.matches(path, is_dir))
            && self.mime.as_ref().is_none_or(|m| m.matches(mime))
            && (self.url.is_some() || self.mime.is_some())
    }
}

/// A `[plugin] previewers` rule: which previewer handles which files.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewRule {
    pub url: Option<UrlPattern>,
    pub mime: Option<MimePattern>,
    pub run: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub mgr: Mgr,
    pub preview: PreviewCfg,
    pub previewers: Vec<PreviewRule>,
    pub spotters: Vec<PreviewRule>,
    pub fetchers: Vec<PreviewRule>,
    pub openers: HashMap<String, Vec<Opener>>,
    pub open_rules: Vec<OpenRule>,
    pub titles: HashMap<String, String>,
    pub warnings: Vec<String>,
}

fn get<'a>(t: &'a Table, path: &[&str]) -> Option<&'a Value> {
    let mut cur = t.get(path[0])?;
    for k in &path[1..] {
        cur = cur.as_table()?.get(*k)?;
    }
    Some(cur)
}

fn get_bool(t: &Table, path: &[&str], default: bool) -> bool {
    get(t, path).and_then(Value::as_bool).unwrap_or(default)
}

fn get_int(t: &Table, path: &[&str], default: i64) -> i64 {
    get(t, path).and_then(Value::as_integer).unwrap_or(default)
}

fn get_str(t: &Table, path: &[&str], default: &str) -> String {
    get(t, path)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

impl Config {
    pub fn load() -> Self {
        let (t, warning) = load_layered(PRESET_YAZI, "yazi.toml");
        let mut cfg = Self::from_table(&t);
        cfg.warnings.extend(warning);
        cfg
    }

    pub fn from_table(t: &Table) -> Self {
        let ratio = get(t, &["mgr", "ratio"])
            .and_then(Value::as_array)
            .map(|a| {
                let v: Vec<u16> = a
                    .iter()
                    .filter_map(|x| x.as_integer())
                    .map(|x| x.clamp(0, 100) as u16)
                    .collect();
                if v.len() == 3 {
                    [v[0], v[1], v[2]]
                } else {
                    [1, 4, 3]
                }
            })
            .unwrap_or([1, 4, 3]);
        let sort = SortOpts {
            by: SortBy::parse(&get_str(t, &["mgr", "sort_by"], "alphabetical"))
                .unwrap_or(SortBy::Alphabetical),
            sensitive: get_bool(t, &["mgr", "sort_sensitive"], false),
            reverse: get_bool(t, &["mgr", "sort_reverse"], false),
            dir_first: get_bool(t, &["mgr", "sort_dir_first"], true),
        };
        let mouse = get(t, &["mgr", "mouse_events"])
            .and_then(Value::as_array)
            .is_none_or(|a| !a.is_empty());
        let mgr = Mgr {
            ratio,
            sort,
            linemode: get_str(t, &["mgr", "linemode"], "none"),
            show_hidden: get_bool(t, &["mgr", "show_hidden"], false),
            show_symlink: get_bool(t, &["mgr", "show_symlink"], true),
            scrolloff: get_int(t, &["mgr", "scrolloff"], 5).max(0) as usize,
            mouse,
            title_format: get_str(t, &["mgr", "title_format"], "wisp: {cwd}"),
        };
        let preview = PreviewCfg {
            wrap: get_str(t, &["preview", "wrap"], "no") == "yes",
            tab_size: get_int(t, &["preview", "tab_size"], 2).clamp(1, 16) as usize,
            max_width: get_int(t, &["preview", "max_width"], 600).max(16) as u32,
            max_height: get_int(t, &["preview", "max_height"], 900).max(16) as u32,
            image_quality: get_int(t, &["preview", "image_quality"], 75).clamp(50, 95) as u8,
        };

        let mut warnings = Vec::new();
        let mut openers = HashMap::new();
        if let Some(Value::Table(ot)) = t.get("opener") {
            for (name, v) in ot {
                let list: Vec<Opener> = match v.clone().try_into::<Vec<Opener>>() {
                    Ok(l) => l,
                    Err(e) => {
                        warnings.push(format!("opener.{name}: {e}"));
                        continue;
                    }
                };
                openers.insert(
                    name.clone(),
                    list.into_iter().filter(Opener::applies).collect(),
                );
            }
        }

        let mut open_rules = Vec::new();
        if let Some(rules) = get(t, &["open", "rules"]).and_then(Value::as_array) {
            for r in rules {
                let Some(r) = r.as_table() else { continue };
                let url = r
                    .get("url")
                    .or_else(|| r.get("name"))
                    .and_then(Value::as_str)
                    .and_then(UrlPattern::new);
                let mime = r
                    .get("mime")
                    .and_then(Value::as_str)
                    .and_then(MimePattern::new);
                let uses = match r.get("use") {
                    Some(Value::String(s)) => vec![s.clone()],
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect(),
                    _ => Vec::new(),
                };
                open_rules.push(OpenRule { url, mime, uses });
            }
        }

        let mut titles = HashMap::new();
        for section in ["input", "confirm", "pick"] {
            if let Some(Value::Table(s)) = t.get(section) {
                for (k, v) in s {
                    match v {
                        Value::String(st) if k.ends_with("_title") || k.ends_with("_body") => {
                            titles.insert(format!("{section}.{k}"), st.clone());
                        }
                        Value::Array(a) if k.ends_with("_title") => {
                            for (i, x) in a.iter().enumerate() {
                                if let Some(st) = x.as_str() {
                                    titles.insert(format!("{section}.{k}.{i}"), st.to_string());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let rules = |key: &str| -> Vec<PreviewRule> {
            let mut out = Vec::new();
            if let Some(list) = get(t, &["plugin", key]).and_then(Value::as_array) {
                for r in list {
                    let Some(r) = r.as_table() else { continue };
                    let url = r
                        .get("url")
                        .or_else(|| r.get("name"))
                        .and_then(Value::as_str)
                        .and_then(UrlPattern::new);
                    let mime = r
                        .get("mime")
                        .and_then(Value::as_str)
                        .and_then(MimePattern::new);
                    let run = r
                        .get("run")
                        .or_else(|| r.get("exec"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if (url.is_some() || mime.is_some()) && !run.is_empty() {
                        out.push(PreviewRule { url, mime, run });
                    }
                }
            }
            out
        };
        let previewers = rules("previewers");
        let spotters = rules("spotters");
        let fetchers = rules("fetchers");

        Self {
            mgr,
            preview,
            previewers,
            spotters,
            fetchers,
            openers,
            open_rules,
            titles,
            warnings,
        }
    }

    /// A configured popup title, e.g. `title("input", "create_title", Some(1))`.
    pub fn title(&self, section: &str, key: &str, idx: Option<usize>) -> String {
        let k = match idx {
            Some(i) => format!("{section}.{key}.{i}"),
            None => format!("{section}.{key}"),
        };
        self.titles
            .get(&k)
            .or_else(|| self.titles.get(&format!("{section}.{key}")))
            .cloned()
            .unwrap_or_default()
    }

    /// The previewer (`run` of the first matching `[plugin] previewers` rule).
    pub fn previewer_for(&self, path: &Path, is_dir: bool, mime: &str) -> &str {
        Self::first_rule(&self.previewers, path, is_dir, mime)
    }

    /// The spotter for a file (`[plugin] spotters`).
    pub fn spotter_for(&self, path: &Path, is_dir: bool, mime: &str) -> &str {
        Self::first_rule(&self.spotters, path, is_dir, mime)
    }

    fn first_rule<'a>(rules: &'a [PreviewRule], path: &Path, is_dir: bool, mime: &str) -> &'a str {
        rules
            .iter()
            .find(|r| {
                r.url.as_ref().is_none_or(|u| u.matches(path, is_dir))
                    && r.mime
                        .as_ref()
                        .is_none_or(|m| !mime.is_empty() && m.matches(mime))
            })
            .map(|r| r.run.as_str())
            .unwrap_or("")
    }

    /// The openers that apply to a file, in rule order, de-duplicated.
    pub fn openers_for(&self, path: &Path, is_dir: bool, mime: &str) -> Vec<Opener> {
        let mut out: Vec<Opener> = Vec::new();
        for rule in &self.open_rules {
            if !rule.matches(path, is_dir, mime) {
                continue;
            }
            for name in &rule.uses {
                if let Some(list) = self.openers.get(name) {
                    for o in list {
                        if !out.iter().any(|x| x.run == o.run) {
                            out.push(o.clone());
                        }
                    }
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out
    }
}

/// Where thumbnails and other caches live.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("wisp")
}

/// Where the session state (e.g. remembered cursor) lives.
pub fn state_dir() -> Result<PathBuf> {
    let d = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("no state dir")?
        .join("wisp");
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_load() {
        let (t, _) = (PRESET_YAZI.parse::<Table>().unwrap(), ());
        let c = Config::from_table(&t);
        assert_eq!(c.mgr.ratio, [1, 4, 3]);
        assert!(c.openers.contains_key("edit"));
        let ops = c.openers_for(Path::new("/tmp/a.txt"), false, "text/plain");
        assert!(ops[0].run.contains("EDITOR"));
        let ops = c.openers_for(Path::new("/tmp/d"), true, "inode/directory");
        assert!(!ops.is_empty());
    }

    #[test]
    fn merge_prepend() {
        let mut base: Table = "[mgr]\nkeymap = [1, 2]\nratio = [1,4,3]".parse().unwrap();
        let user: Table = "[mgr]\nprepend_keymap = [0]\nappend_keymap = [3]\nratio = [1,1,1]"
            .parse()
            .unwrap();
        merge(&mut base, user);
        let km = base["mgr"]["keymap"].as_array().unwrap();
        assert_eq!(km.len(), 4);
        assert_eq!(km[0].as_integer(), Some(0));
        assert_eq!(base["mgr"]["ratio"][0].as_integer(), Some(1));
    }

    #[test]
    fn url_patterns() {
        let p = UrlPattern::new("*/").unwrap();
        assert!(p.matches(Path::new("/a/b"), true));
        assert!(!p.matches(Path::new("/a/b"), false));
        let p = UrlPattern::new("*.{AppImage,appimage}").unwrap();
        assert!(p.matches(Path::new("/x/y.AppImage"), false));
        let m = MimePattern::new("application/{zip,7z*}").unwrap();
        assert!(m.matches("application/7z-compressed"));
    }
}

// --------------------------------------------------------------------------
// Compiled-config cache
// --------------------------------------------------------------------------

/// A file the compiled config depends on, with the stamp it had.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Dep {
    path: PathBuf,
    stamp: Option<(i64, u32, u64)>,
}

fn stamp(p: &Path) -> Option<(i64, u32, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(p).ok()?;
    Some((m.mtime(), m.mtime_nsec() as u32, m.len()))
}

#[derive(Serialize, Deserialize)]
struct Compiled {
    version: u64,
    /// The config search path the cache was built for (env vars can change it).
    dirs: Vec<PathBuf>,
    deps: Vec<Dep>,
    cfg: Config,
    th: crate::theme::Theme,
    km: crate::keymap::Keymap,
}

/// FNV-1a, usable in `const` context so the presets are hashed at compile time.
const fn fnv(mut h: u64, bytes: &[u8]) -> u64 {
    let mut i = 0;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(0x100000001b3);
        i += 1;
    }
    h
}

/// Bump when the layout or invariants of the cached structures change.
const CACHE_FORMAT: &str = "cache-format-5";

const BUILD_VERSION: u64 = {
    let h = fnv(0xcbf29ce484222325, env!("CARGO_PKG_VERSION").as_bytes());
    let h = fnv(h, CACHE_FORMAT.as_bytes());
    let h = fnv(h, PRESET_YAZI.as_bytes());
    let h = fnv(h, PRESET_KEYMAP.as_bytes());
    fnv(h, PRESET_THEME.as_bytes())
};

fn build_version() -> u64 {
    BUILD_VERSION
}

fn candidate_deps() -> Vec<PathBuf> {
    let mut v = Vec::new();
    for d in config_dirs() {
        for f in ["yazi.toml", "keymap.toml", "theme.toml"] {
            v.push(d.join(f));
        }
    }
    v
}

/// Load config, theme and keymap. Parsing ~2000 lines of TOML costs more
/// than everything else at startup combined, so the compiled result is cached
/// and reused until any of the files it came from changes.
pub fn load_all() -> (Config, crate::theme::Theme, crate::keymap::Keymap) {
    let cache = cache_dir().join("config.bin");
    let version = build_version();
    let bincfg = bincode::config::standard();
    crate::trace("cache: start");
    if let Ok(bytes) = std::fs::read(&cache) {
        crate::trace("cache: read");
        if let Ok((c, _)) = bincode::serde::decode_from_slice::<Compiled, _>(&bytes, bincfg) {
            crate::trace("cache: decoded");
            if c.version == version
                && c.dirs == config_dirs()
                && c.deps.iter().all(|d| stamp(&d.path) == d.stamp)
            {
                crate::trace("cache: validated");
                return (c.cfg, c.th, c.km);
            }
        }
    }
    // Miss: parse the three files in parallel.
    let (cfg, th, km) = std::thread::scope(|s| {
        let a = s.spawn(Config::load);
        let b = s.spawn(crate::theme::Theme::load);
        let km = crate::keymap::Keymap::load();
        (a.join().unwrap(), b.join().unwrap(), km)
    });
    let mut deps: Vec<Dep> = candidate_deps()
        .into_iter()
        .chain(th.deps.iter().cloned())
        .map(|p| {
            let stamp = stamp(&p);
            Dep { path: p, stamp }
        })
        .collect();
    deps.dedup_by(|a, b| a.path == b.path);
    let compiled = Compiled {
        version,
        dirs: config_dirs(),
        deps,
        cfg,
        th,
        km,
    };
    if let Ok(bytes) = bincode::serde::encode_to_vec(&compiled, bincfg) {
        let _ = std::fs::create_dir_all(cache_dir());
        let tmp = cache.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &cache);
        }
    }
    (compiled.cfg, compiled.th, compiled.km)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn compiled_roundtrip() {
        let t: Table = PRESET_YAZI.parse().unwrap();
        let cfg = Config::from_table(&t);
        let th = crate::theme::Theme::from_table(&PRESET_THEME.parse().unwrap());
        let km = crate::keymap::Keymap::from_table(&PRESET_KEYMAP.parse().unwrap());
        let c = Compiled {
            version: 1,
            dirs: vec![],
            deps: vec![],
            cfg,
            th,
            km,
        };
        let bytes = bincode::serde::encode_to_vec(&c, bincode::config::standard()).unwrap();
        let (back, _): (Compiled, _) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(back.cfg.mgr.ratio, [1, 4, 3]);
        assert_eq!(back.th.s("mgr.cwd"), c.th.s("mgr.cwd"));
        assert_eq!(
            back.th.file_style(Path::new("/x.png"), "image/png", 0),
            c.th.file_style(Path::new("/x.png"), "image/png", 0)
        );
        assert_eq!(
            back.km.bindings(crate::keymap::Layer::Mgr).len(),
            c.km.bindings(crate::keymap::Layer::Mgr).len()
        );
    }
}

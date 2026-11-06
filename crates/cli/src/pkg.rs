//! `wisp pkg` — install and manage yazi plugins and flavors, like
//! `ya pkg`. Packages are fetched with `git` and recorded in `package.toml`.
//!
//! ```text
//! wisp pkg add yazi-rs/plugins:git      # one plugin from a monorepo
//! wisp pkg add owner/foo                # a whole repo (foo.yazi)
//! wisp pkg list
//! wisp pkg upgrade
//! wisp pkg delete git
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use toml::{Table, Value};

/// Where packages go: the first existing config dir, else wisp's own.
fn home() -> PathBuf {
    let dirs = crate::config::config_dirs();
    dirs.iter()
        .find(|d| d.join("plugins").is_dir() || d.join("package.toml").is_file())
        .or_else(|| dirs.first())
        .cloned()
        .unwrap_or_else(|| PathBuf::from("."))
}

struct Spec {
    repo: String,
    sub: Option<String>,
}

impl Spec {
    fn parse(s: &str) -> Result<Self> {
        let (repo, sub) = match s.split_once(':') {
            Some((r, p)) => (r.to_string(), Some(p.to_string())),
            None => (s.to_string(), None),
        };
        if repo.split('/').count() != 2 {
            bail!("expected `owner/repo` or `owner/repo:name`, got `{s}`");
        }
        Ok(Self { repo, sub })
    }

    /// The installed directory name, e.g. `git.yazi`.
    fn dir_name(&self) -> String {
        let base = match &self.sub {
            Some(s) => s.clone(),
            None => self.repo.rsplit('/').next().unwrap_or("").to_string(),
        };
        let base = base.trim_end_matches(".yazi").to_string();
        format!("{base}.yazi")
    }

    fn id(&self) -> String {
        match &self.sub {
            Some(s) => format!("{}:{s}", self.repo),
            None => self.repo.clone(),
        }
    }
}

fn fetch(spec: &Spec) -> Result<(PathBuf, String, tempdir::Dir)> {
    let tmp = tempdir::Dir::new()?;
    let url = format!("https://github.com/{}.git", spec.repo);
    let st = Command::new("git")
        .args(["clone", "--quiet", "--depth", "1", &url])
        .arg(tmp.path().join("repo"))
        .status()
        .context("running git (is it installed?)")?;
    if !st.success() {
        bail!("git clone {url} failed");
    }
    let repo = tmp.path().join("repo");
    let rev = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let src = match &spec.sub {
        Some(s) => {
            let a = repo.join(format!("{}.yazi", s.trim_end_matches(".yazi")));
            if a.is_dir() {
                a
            } else {
                repo.join(s)
            }
        }
        None => repo.clone(),
    };
    if !src.is_dir() {
        bail!(
            "`{}` not found in {}",
            spec.sub.as_deref().unwrap_or(""),
            spec.repo
        );
    }
    Ok((src, rev, tmp))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".git" {
            continue;
        }
        let dst = to.join(&name);
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dst)?;
        } else {
            std::fs::copy(e.path(), dst)?;
        }
    }
    Ok(())
}

/// Plugins go to `plugins/`, flavors (they ship a `flavor.toml`) to `flavors/`.
fn kind_dir(src: &Path) -> &'static str {
    if src.join("flavor.toml").is_file() {
        "flavors"
    } else {
        "plugins"
    }
}

fn load_manifest(path: &Path) -> Table {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

fn deps(t: &mut Table, kind: &str) -> Vec<Table> {
    t.get(kind)
        .and_then(|v| v.get("deps"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_table().cloned()).collect())
        .unwrap_or_default()
}

fn set_deps(t: &mut Table, kind: &str, list: Vec<Table>) {
    let mut sec = Table::new();
    sec.insert(
        "deps".into(),
        Value::Array(list.into_iter().map(Value::Table).collect()),
    );
    t.insert(kind.into(), Value::Table(sec));
}

fn install(spec: &Spec, home: &Path) -> Result<(String, String)> {
    let (src, rev, _tmp) = fetch(spec)?;
    let kind = kind_dir(&src);
    let dst = home.join(kind).join(spec.dir_name());
    if dst.exists() {
        std::fs::remove_dir_all(&dst)?;
    }
    copy_dir(&src, &dst)?;
    Ok((kind.to_string(), rev))
}

pub fn run(args: &[String]) -> Result<()> {
    let home = home();
    let manifest = home.join("package.toml");
    let mut t = load_manifest(&manifest);
    match args.first().map(String::as_str) {
        Some("add") | Some("install") => {
            if args.len() < 2 {
                bail!("usage: wisp pkg add <owner/repo[:name]>...");
            }
            for a in &args[1..] {
                let spec = Spec::parse(a)?;
                let (kind, rev) = install(&spec, &home)?;
                let section = if kind == "flavors" {
                    "flavor"
                } else {
                    "plugin"
                };
                let mut list = deps(&mut t, section);
                list.retain(|d| d.get("use").and_then(Value::as_str) != Some(&spec.id()));
                let mut d = Table::new();
                d.insert("use".into(), Value::String(spec.id()));
                d.insert("rev".into(), Value::String(rev.clone()));
                list.push(d);
                set_deps(&mut t, section, list);
                println!(
                    "Installed {} ({rev}) into {}",
                    spec.id(),
                    home.join(&kind).display()
                );
            }
        }
        Some("list") | None => {
            for section in ["plugin", "flavor"] {
                for d in deps(&mut t, section) {
                    println!(
                        "{section:<7} {:<40} {}",
                        d.get("use").and_then(Value::as_str).unwrap_or(""),
                        d.get("rev").and_then(Value::as_str).unwrap_or("")
                    );
                }
            }
        }
        Some("upgrade") | Some("update") => {
            for section in ["plugin", "flavor"] {
                let mut list = deps(&mut t, section);
                for d in &mut list {
                    let Some(id) = d.get("use").and_then(Value::as_str).map(String::from) else {
                        continue;
                    };
                    let spec = Spec::parse(&id)?;
                    let (_, rev) = install(&spec, &home)?;
                    println!("Upgraded {id} → {rev}");
                    d.insert("rev".into(), Value::String(rev));
                }
                if !list.is_empty() {
                    set_deps(&mut t, section, list);
                }
            }
        }
        Some("delete") | Some("remove") => {
            let Some(name) = args.get(1) else {
                bail!("usage: wisp pkg delete <name>");
            };
            let name = name.trim_end_matches(".yazi");
            let mut removed = false;
            for (section, dir) in [("plugin", "plugins"), ("flavor", "flavors")] {
                let mut list = deps(&mut t, section);
                let before = list.len();
                list.retain(|d| {
                    let id = d.get("use").and_then(Value::as_str).unwrap_or("");
                    let dir = Spec::parse(id).map(|s| s.dir_name()).unwrap_or_default();
                    dir != format!("{name}.yazi")
                });
                if list.len() != before {
                    set_deps(&mut t, section, list);
                }
                let p = home.join(dir).join(format!("{name}.yazi"));
                if p.is_dir() {
                    std::fs::remove_dir_all(&p)?;
                    removed = true;
                    println!("Deleted {}", p.display());
                }
            }
            if !removed {
                bail!("no package named `{name}`");
            }
        }
        Some(other) => bail!("unknown pkg command `{other}` (add, list, upgrade, delete)"),
    }
    std::fs::create_dir_all(&home)?;
    std::fs::write(&manifest, toml::to_string(&t)?)?;
    Ok(())
}

/// A self-deleting temporary directory (std has none; avoids a dependency).
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct Dir(PathBuf);

    impl Dir {
        pub fn new() -> std::io::Result<Self> {
            let p = std::env::temp_dir().join(format!(
                "wisp-pkg-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&p)?;
            Ok(Self(p))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        let s = Spec::parse("yazi-rs/plugins:git").unwrap();
        assert_eq!(s.dir_name(), "git.yazi");
        assert_eq!(s.id(), "yazi-rs/plugins:git");
        let s = Spec::parse("owner/foo.yazi").unwrap();
        assert_eq!(s.dir_name(), "foo.yazi");
        assert!(Spec::parse("nope").is_err());
    }
}

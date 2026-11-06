//! A small glob matcher for yazi-style rules (`*.{jpg,png}`, `application/7z*`,
//! `[Mm]akefile`). Unlike a regex-backed glob it costs microseconds to build,
//! which matters because a theme carries dozens of rules and startup time is
//! the one thing a file manager must never waste.
//!
//! Semantics: `*` and `**` match any run of characters (including `/`), `?`
//! matches one character, `[abc]`/`[a-z]`/`[!a]` match a class, `{a,b}`
//! alternates (nesting allowed), `\` escapes. Matching is case-insensitive.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Lit(char),
    One,
    Star,
    Class { neg: bool, items: Vec<(char, char)> },
}

#[derive(Clone, Debug)]
pub struct Glob {
    src: String,
    alts: Vec<Vec<Tok>>,
    /// `*` alone: matches everything, skip the work.
    any: bool,
}

fn lower(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Expand the first top-level `{a,b}` group recursively.
fn expand(p: &str) -> Vec<String> {
    let chars: Vec<char> = p.chars().collect();
    let mut depth = 0;
    let mut open = None;
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '{' => {
                if depth == 0 {
                    open = Some(i);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    let o = open.unwrap();
                    let pre: String = chars[..o].iter().collect();
                    let post: String = chars[i + 1..].iter().collect();
                    // Split the inside on top-level commas.
                    let mut parts = Vec::new();
                    let mut cur = String::new();
                    let mut d = 0;
                    let mut k = o + 1;
                    while k < i {
                        let c = chars[k];
                        match c {
                            '\\' => {
                                cur.push(c);
                                if k + 1 < i {
                                    cur.push(chars[k + 1]);
                                }
                                k += 1;
                            }
                            '{' => {
                                d += 1;
                                cur.push(c);
                            }
                            '}' => {
                                d -= 1;
                                cur.push(c);
                            }
                            ',' if d == 0 => parts.push(std::mem::take(&mut cur)),
                            _ => cur.push(c),
                        }
                        k += 1;
                    }
                    parts.push(cur);
                    let mut out = Vec::new();
                    for part in parts {
                        for e in expand(&format!("{pre}{part}{post}")) {
                            out.push(e);
                        }
                    }
                    return out;
                }
            }
            _ => {}
        }
        i += 1;
    }
    vec![p.to_string()]
}

fn tokenize(p: &str) -> Vec<Tok> {
    let chars: Vec<char> = p.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                out.push(Tok::Lit(lower(chars[i + 1])));
                i += 2;
                continue;
            }
            '*' => {
                if out.last() != Some(&Tok::Star) {
                    out.push(Tok::Star);
                }
            }
            '?' => out.push(Tok::One),
            '[' => {
                // Find the closing bracket; a `]` right after `[` or `[!` is literal.
                let mut j = i + 1;
                let neg = j < chars.len() && (chars[j] == '!' || chars[j] == '^');
                if neg {
                    j += 1;
                }
                let start = j;
                while j < chars.len() && (chars[j] != ']' || j == start) {
                    j += 1;
                }
                if j >= chars.len() {
                    out.push(Tok::Lit('['));
                } else {
                    let body = &chars[start..j];
                    let mut items = Vec::new();
                    let mut k = 0;
                    while k < body.len() {
                        if k + 2 < body.len() && body[k + 1] == '-' {
                            items.push((lower(body[k]), lower(body[k + 2])));
                            k += 3;
                        } else {
                            items.push((lower(body[k]), lower(body[k])));
                            k += 1;
                        }
                    }
                    out.push(Tok::Class { neg, items });
                    i = j + 1;
                    continue;
                }
            }
            c => out.push(Tok::Lit(lower(c))),
        }
        i += 1;
    }
    out
}

fn tok_matches(t: &Tok, c: char) -> bool {
    match t {
        Tok::Lit(l) => *l == c,
        Tok::One => true,
        Tok::Class { neg, items } => {
            let hit = items.iter().any(|(a, b)| c >= *a && c <= *b);
            hit != *neg
        }
        Tok::Star => true,
    }
}

/// Classic wildcard matching with single-point backtracking (enough because
/// `*` is unrestricted).
fn wild(p: &[Tok], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            if p[pi] == Tok::Star {
                star = Some((pi, si));
                pi += 1;
                continue;
            }
            if tok_matches(&p[pi], s[si]) {
                pi += 1;
                si += 1;
                continue;
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == Tok::Star {
        pi += 1;
    }
    pi == p.len()
}

impl Glob {
    pub fn new(src: &str) -> Self {
        let alts = expand(src).iter().map(|a| tokenize(a)).collect();
        Self {
            src: src.to_string(),
            alts,
            any: src == "*" || src == "**",
        }
    }

    pub fn is_match(&self, s: &str) -> bool {
        if self.any {
            return true;
        }
        // Rules run for every visible row on every frame: avoid allocating
        // for the (overwhelmingly common) short, ASCII-ish input.
        let mut stack = [' '; 256];
        let n = s.chars().count();
        if n <= stack.len() {
            for (slot, c) in stack.iter_mut().zip(s.chars()) {
                *slot = lower(c);
            }
            let chars = &stack[..n];
            return self.alts.iter().any(|a| wild(a, chars));
        }
        let chars: Vec<char> = s.chars().map(lower).collect();
        self.alts.iter().any(|a| wild(a, &chars))
    }
}

impl Serialize for Glob {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.src)
    }
}

impl<'de> Deserialize<'de> for Glob {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(Glob::new(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(Glob::new("*.rs").is_match("/a/b/main.rs"));
        assert!(!Glob::new("*.rs").is_match("/a/b/main.rsx"));
        assert!(Glob::new("*.{jpg,png}").is_match("x.PNG"));
        assert!(Glob::new("application/{zip,7z*}").is_match("application/7z-compressed"));
        assert!(!Glob::new("application/{zip,7z*}").is_match("application/json"));
        assert!(Glob::new("{audio,video}/*").is_match("video/mp4"));
        assert!(Glob::new("image/hei?").is_match("image/heic"));
        assert!(Glob::new("[Mm]akefile").is_match("makefile"));
        assert!(Glob::new("[!a]bc").is_match("xbc"));
        assert!(!Glob::new("[!a]bc").is_match("abc"));
        assert!(Glob::new("*").is_match("anything/at/all"));
        assert!(Glob::new("**/Downloads/*").is_match("/home/x/Downloads/f"));
        assert!(Glob::new("a{b,c{d,e}}f").is_match("acef"));
        assert!(Glob::new("a\\*b").is_match("a*b"));
        assert!(!Glob::new("a\\*b").is_match("axb"));
    }
}

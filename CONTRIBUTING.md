# Contributing to Wisp

Thanks for taking a look! This is a small, dependency-light Rust workspace, so
getting set up takes one command.

## Prerequisites

- A recent stable Rust toolchain (`rustup` recommended). Install from
  <https://rustup.rs>.
- No system libraries are required. Image previews use the terminal's own
  graphics protocol; the optional `chafa` backend is deliberately disabled.
- Optional, for richer previews at runtime: `ffmpegthumbnailer` (video),
  `pdftoppm` (PDF), `magick` (SVG/HEIC/AVIF/JXL, fonts), `bsdtar`/`7z`
  (archives), `file` (fallback descriptions).

## Workspace layout

```
crates/core   wisp-core — full-text indexing + BM25 search engine (library, no UI)
crates/cli    wisp      — the file manager + `index`/`query`/`stats` commands
  preset/     yazi's default yazi.toml / keymap.toml / theme (MIT), our built-in defaults
scripts/      bench.py (startup benchmark vs. yazi), screen.py (render a TUI to text)
```

The file manager lives in `crates/cli/src`:

| module        | what it does                                                        |
|---------------|---------------------------------------------------------------------|
| `app.rs`      | state, main loop, loading, watching, terminal hand-off              |
| `commands.rs` | every keymap command (yazi's names and flags), per layer            |
| `ui.rs`       | rendering; one function per yazi UI component                       |
| `config.rs`   | yazi-format config, layering + the compiled-config cache            |
| `theme.rs`    | `theme.toml`, flavors, filetype rules, icons                        |
| `keymap.rs`   | keys, chords, commands                                              |
| `fs.rs`       | entries, parallel directory reads, sorting, MIME detection          |
| `folder.rs`, `tab.rs` | a listing with cursor/filter; a tab with history/selection  |
| `preview.rs`  | off-thread previews with an LRU and neighbour prefetch              |
| `tasks.rs`    | copy/move/link/trash/delete/extract/process tasks with progress     |
| `search.rs`   | in-process `fd`/`rg`-style search, and deep search via the index    |
| `input.rs`    | the vi-style line editor used by every prompt                       |
| `glob.rs`     | the small, fast glob matcher used by all rules                      |
| `plugin.rs`, `plugin_prelude.lua`, `plugin_ui.lua` | the yazi-compatible Lua plugin runtime: Rust primitives, the `ya`/`Command`/`Url`/`fs`/`ps`/`cx` API, and the `ui`/`th`/component layer rendered natively |
| `pkg.rs`      | `wisp pkg` (like `ya pkg`)                                    |
| `remote.rs`   | `wisp emit` / `emit-to` (like `ya emit`) over a Unix socket   |

## Everyday commands

```bash
cargo build                          # debug build
cargo build --profile quick          # optimized, fast to rebuild (for trying things)
cargo run -- ~/some/dir              # run the file manager

# Tests — unit tests live inline in each module; app.rs has an end-to-end test
cargo test

# Lints & formatting — please run both before opening a PR
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all

# Compare against yazi (needs yazi installed; screen.py needs `pip install pyte`)
python3 scripts/bench.py --dir /some/dir --marker some-file -- target/release/wisp
python3 scripts/bench.py --dir /some/dir --marker some-file -- yazi
python3 scripts/screen.py --dir . --keys 'jl' -- "$PWD/target/release/wisp"
```

## Before you open a PR

1. `cargo test` passes.
2. `cargo clippy --workspace --all-targets -- -D warnings` is clean.
3. `cargo fmt --all` has been run.
4. New behaviour has a test (`#[cfg(test)] mod tests` at the bottom of the
   file, `tempfile` for anything touching the filesystem).
5. UI changes keep parity with yazi: compare with `scripts/screen.py`.

## Releasing

Releases are automated. Bump `version` in the workspace `Cargo.toml`, commit,
then tag and push:

```bash
git tag v0.4.0
git push origin v0.4.0
```

The `Release` workflow builds binaries for Linux and macOS (Intel + Apple
Silicon) and attaches them (with `.sha256` checksums) to a GitHub Release for
that tag. `CI` runs fmt + clippy + tests on every push and PR.

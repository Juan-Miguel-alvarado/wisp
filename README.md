# Wisp

[![Release](https://img.shields.io/github/v/release/Juan-Miguel-alvarado/Deepsearch?sort=semver)](https://github.com/Juan-Miguel-alvarado/Deepsearch/releases/latest)

A terminal file manager with [yazi](https://github.com/sxyazi/yazi)'s UI, keymap
and config format — **several times faster to start, far lighter on memory, and
an order of magnitude faster on huge directories** — plus deep full-text search
inside your documents (PDF, DOCX, code, text).

If you know yazi you already know Wisp: same three-column layout, same
keys, same popups, and it reads your existing `~/.config/yazi/` theme, keymap
and openers as they are.

```
wisp            # open the file manager in the current directory
wisp ~/photos   # …or somewhere else (pass a file to reveal it)
```

---

## How much faster

Measured on the same machine against yazi 26.5.6, both reading the same user
config, in a 160×40 pseudo-terminal. "First paint" is the time from launching
the program until the directory listing is on screen
([`scripts/bench.py`](scripts/bench.py); medians of 5–30 runs each).

| | Wisp | yazi | |
|---|---:|---:|---:|
| First paint, small directory (50 files) | **7.8 ms** | 47.4 ms | **6.1× faster** |
| First paint, `/usr/bin` (2,988 entries) | **13.0 ms** | 80.2 ms | **6.2× faster** |
| First paint, 100 000 files | **113 ms** | 1,759 ms | **15× faster** |
| Jump to the last of 100 000 files (`G`) | **8.9 ms** | 104 ms | **12× faster** |
| Memory after first paint, small directory | **8.2 MiB** | 59.8 MiB | **7.3× less** |
| Memory after first paint, `/usr/bin` | **9.5 MiB** | 88.0 MiB | **9.3× less** |
| Memory with 100 000 files listed | **48 MiB** | 164 MiB | **3.4× less** |

<sub>The pseudo-terminal harness itself adds ~3.6 ms (that is how long a
program that only prints the expected text takes to "paint"), so the real
difference you feel is larger than the ratios above. Reproduce with
`python3 scripts/bench.py --dir <dir> --marker <a-file-name> -- <program>`.</sub>

Where the speed comes from:

- **No interpreter at start-up.** yazi boots a Lua runtime and parses its TOML
  presets on every launch. Wisp compiles config, theme and keymap once,
  caches the result in a compact binary form (invalidated whenever any of the
  source files change), and decodes it on a helper thread while the terminal is
  being set up and the starting directory is read on another.
- **One pass over the directory.** Names and `statx` data are read together and
  fanned out across cores for large directories; sorting is parallel; only the
  visible rows are ever rendered.
- **No process per file.** MIME types come from the extension (a table lookup);
  content sniffing runs only for files without a known extension, off the UI
  thread. yazi spawns `file(1)` for this.
- **In-process search.** `s`/`S` use the same libraries as `fd` and ripgrep, but
  without spawning them — and they work even when neither is installed.
- **Idle means idle.** The UI thread blocks on a single event channel and only
  repaints when something changed; previews are built off-thread with an LRU and
  neighbour prefetching, so moving the cursor back and forth is instant.

## Everything yazi does

- **Layout & look** — parent / current / preview columns with yazi's ratios,
  header with cwd and selection/yank counters, tab bar, status bar with mode,
  size, name, permissions, position and task progress. Rounded popups in the
  same places, same nerd-font icon table and filetype colours.
- **Navigation** — `hjkl`/arrows, half/full pages, `gg`/`G`, back/forward
  history (`H`/`L`), `cd` with path completion (`g<Space>`), `gh` `gc` `gd`,
  follow symlinks (`gf`), the cursor remembered per directory, `scrolloff`.
- **Selection** — `<Space>`, `<C-a>`, `<C-r>`, visual select/unset modes
  (`v`/`V`), markers in the left gutter.
- **File operations** — yank/cut/paste (`y` `x` `p` `P`), symlinks absolute and
  relative (`-` `_`), hardlinks (`<C-->`), trash (`d`) and permanent delete (`D`)
  with confirmation, create files/directories (`a`, trailing `/` for a dir),
  rename with the cursor before the extension (`r`), **bulk rename in `$EDITOR`**
  when several files are selected, archive extraction. All copies/moves run as
  background tasks with progress, cancellable from the task manager (`w`).
  Yanks are shared between running instances, like yazi.
- **Opening** — yazi's `[opener]`/`[open]` rules, the "Open with" picker (`O`),
  blocking openers that hand over the terminal (`$EDITOR`), orphaned GUI apps.
- **Shell** — `;` and `:` with `$0`/`$@` and yazi's `%s`/`%s1`/`%d`/`%h`
  placeholders.
- **Find, filter, search** — incremental find with `n`/`N` and `[i/n]`
  counters (`/`, `?`), live filter (`f`), recursive search by name (`s`) and by
  content (`S`), cancel with `<C-s>`.
- **Previews** — syntax-highlighted code, images (Kitty, Sixel, iTerm2 or
  half-blocks), video thumbnails, PDF pages, SVG/HEIC/AVIF/JXL, fonts, archive
  listings, `file(1)` descriptions for the rest; scroll them with `J`/`K`.
- **Spot** (`<Tab>`) — yazi's table (Folder/Image/Video, Base, Plugins
  sections), with copy-a-cell and swipe between files.
- **Sorting & linemodes** — `,m` `,b` `,e` `,a` `,n` `,s` `,r` (and reversed),
  `ms` `mp` `mb` `mm` `mo` `mn`; hidden files (`.`); copy path / dir / name
  (`cc` `cd` `cf` `cn`, via `wl-copy`/`xclip`/OSC 52).
- **Tabs** — `tt`, `tr`, `1`–`9`, `[` `]`, `{` `}`, `<C-c>`.
- **Plugins** — yazi's Lua plugins and `init.lua`, `ya pkg`-style package
  management (`wisp pkg`), `ya emit`-style remote commands
  (`wisp emit`), `[plugin] previewers` rules including `piper`.
  See [Plugins](#plugins).
- **Everything else** — which-key popup for chords, help with filter (`~`),
  notifications, mouse (click, right-click to open, scroll), live directory
  watching, `<C-z>` suspend, `--cwd-file`, `--chooser-file` (file-picker mode),
  `fzf` (`z`) and `zoxide` (`Z`) jumps.

## …and more

| | |
|---|---|
| **Deep search** (`F`) | Ranked full-text search *inside* PDFs, Word documents, code and text, from a persistent index (`wisp index`). Typo-tolerant, BM25-ranked, sub-millisecond queries. |
| **Directory sizes** (`=`) | Total size of the hovered/selected directories, computed in parallel and shown in the size linemode and status bar. |
| **Maximize preview** (`T`) | Toggle a full-width preview (yazi needs a plugin for this). |
| **Richer spot** | An extra *Details* section: size in bytes, accessed time, octal mode, owner, links, inode, line count, symlink target with broken-link detection. |
| **Find counter while typing** | `[i/n]` updates live as you type the pattern. |
| **No helper tools required** | Search, MIME detection and archive listing for zip work without `fd`, `rg`, `file` or `7z`. |

## Installation

### Prebuilt binary

Grab the archive for your platform from the
[latest release](https://github.com/Juan-Miguel-alvarado/Deepsearch/releases/latest),
extract it and put `wisp` on your `PATH`:

```bash
tar -xzf wisp-x86_64-unknown-linux-gnu.tar.gz
install -m 755 wisp ~/.local/bin/
```

### From source

```bash
cargo install --git https://github.com/Juan-Miguel-alvarado/Deepsearch wisp
# or, from a clone:
cargo build --release && cp target/release/wisp ~/.local/bin/
```

A [Nerd Font](https://www.nerdfonts.com) is needed for icons, as with yazi.
Optional tools for richer previews: `ffmpegthumbnailer` (video), `pdftoppm`
(PDF), `magick` (SVG/HEIC/AVIF/JXL/fonts), `bsdtar` or `7z` (non-zip archives),
`file`, `fzf`, `zoxide`. `wisp --debug` shows which are found and where
your config is read from.

### Change directory on exit

The same wrapper as yazi's: quit with `q` and your shell lands where you were.

```bash
# bash / zsh
function d() {
	local tmp="$(mktemp -t "wisp-cwd.XXXXXX")" cwd
	wisp "$@" --cwd-file="$tmp"
	IFS= read -r -d '' cwd < "$tmp"
	[ -n "$cwd" ] && [ "$cwd" != "$PWD" ] && builtin cd -- "$cwd"
	rm -f -- "$tmp"
}
```

```fish
function d
	set tmp (mktemp -t "wisp-cwd.XXXXXX")
	wisp $argv --cwd-file="$tmp"
	if read -z cwd < "$tmp"; and [ -n "$cwd" ]; and [ "$cwd" != "$PWD" ]
		builtin cd -- "$cwd"
	end
	rm -f -- "$tmp"
end
```

## Configuration

Wisp uses yazi's file format. For each of `yazi.toml`, `keymap.toml` and
`theme.toml` it uses the first one found in:

1. `$WISP_CONFIG_HOME` or `~/.config/wisp/`
2. `$YAZI_CONFIG_HOME` or `~/.config/yazi/`

so an existing yazi setup works unchanged, and you can override single files for
Wisp only. Everything layers over yazi's defaults exactly like in yazi:
tables merge, arrays replace, and `prepend_*` / `append_*` add around the
defaults (e.g. `[mgr] prepend_keymap = [...]`). Flavors
(`[flavor] dark = "…"` → `flavors/<name>.yazi/flavor.toml` + `tmtheme.xml`)
are supported.

Supported from `yazi.toml`: `[mgr]` (`ratio`, `sort_*`, `linemode`,
`show_hidden`, `show_symlink`, `scrolloff`, `mouse_events`, `title_format`),
`[preview]` (`tab_size`, `max_width`, `max_height`, `image_quality`),
`[opener]`, `[open]` rules, and the `[input]`/`[confirm]`/`[pick]` titles.
Every yazi command in `keymap.toml` works, with its flags, in every layer.

## Plugins

yazi's Lua plugins work. Put them where yazi does —
`plugins/<name>.yazi/main.lua` under `~/.config/wisp/` or
`~/.config/yazi/` — or install them like `ya pkg`:

```bash
wisp pkg add yazi-rs/plugins:smart-enter   # one plugin from a monorepo
wisp pkg add owner/some-flavor             # flavors work the same way
wisp pkg list
wisp pkg upgrade
wisp pkg delete smart-enter
```

and bind them as usual (`run = "plugin smart-enter"`). `init.lua` runs after
the first frame, so `require("x"):setup { … }` and `ps.sub("cd", …)` hooks work
without slowing start-up; the Lua runtime (Lua 5.4) is only created when a
plugin or `init.lua` is actually used.

Supported API: `ya.emit` (also `mgr_emit`/`manager_emit`), `ya.notify`,
`ya.input` (including `realtime = true` receivers), `ya.confirm`, `ya.which`,
`ya.hide`, `ya.sync`, `ya.clipboard`, `ya.json_decode`/`json_encode`,
`ya.readable_size`/`readable_path`, `ya.quote`, `ya.hash`, `ya.time`/`sleep`,
`ya.user_name`/`group_name`/`uid`/`gid`/`host_name`, `ya.target_os`;
`Command` with `arg`/`cwd`/`env`/`stdin`/`stdout`/`stderr` and
`output`/`status`/`spawn` (`read_line`, `write_all`, `wait`,
`wait_with_output`, `start_kill`); `Url` (`name`, `stem`, `ext`, `parent`,
`join`, `starts_with`, `ends_with`, `strip_prefix`, `into_search`); `fs.cha`,
`fs.file`, `fs.read_dir`, `fs.write`, `fs.read`, `fs.remove`, `fs.create`,
`fs.rename`, `fs.copy`, `fs.unique_name`, `fs.cwd`; `ps.sub`/`ps.pub` with
`cd`, `hover` and `tab` events; `rt` (including a writable `rt.mgr.ratio`);
and `cx` (tabs, current/parent/preview folders with their files, hovered,
selected, yanked, mode, preferences). Virtual folders built with
`url:into_search()` + `update_files` (as `vcs-files` does) show up as a search.

`[plugin] previewers` rules in `yazi.toml` are honoured, including the popular
`piper` previewer (ANSI colours included):

```toml
[plugin]
prepend_previewers = [
  { url = "*.md", run = 'piper -- glow -s dark -w "$w" "$1"' },
]
```

Plugins that render UI work too:

- **Fetchers** (`[plugin] prepend_fetchers`) run for the visible files, e.g.
  `git.yazi`'s `git status` fetcher.
- **Linemode / Entity / Status / Header children** registered with
  `children_add` are drawn in their slots by `order`, and user-defined methods
  replace the built-in ones (`function Status:name() … end`). Custom linemodes
  (`function Linemode:size_and_mtime() … end` + `linemode = "size_and_mtime"`)
  work.
- **Previewers** (`peek` + `ya.preview_widget`, `ui.Text.parse` for ANSI,
  `ya.image_show`, falling back to `require("code")` & co.) run on the preview
  thread with their own Lua state, so a slow previewer never blocks the UI.
- **Spotters** (`spot` + `ya.spot_table`, `require("file"):spot_base`).
- `ui` (`Style`, `Span`, `Line`, `Text`, `List`, `Table`, `Row`, `Rect`, `Pad`,
  `Layout`, `Constraint`, `Align`, `Wrap`, `width`, `truncate`, `render`) and
  `th` (your theme, including custom sections such as `[git]`).

- **Layout plugins**: wrapping `Tab.layout` / `Tab.build` (borders, resizing
  the tab area), `Root.layout` (rearranging header / tabs / status), and
  `Status.redraw` overrides.
- **List overrides**: `Current.redraw` / `Parent.redraw` replacements run
  against Lua ports of yazi's own `Entity` / `Linemode` / `Current` /
  `Parent`, so wrappers that call the original keep working.
- **Plugin UIs** registered as `Modal` children (e.g. `mount`'s partition
  table): positioned `Clear` / `Border` / `Table` / `List` / `Text` widgets.
- **Concurrency**: `ya.join` / `ya.select` with `ya.chan` channels, including
  `ya.which` / `ya.input` called from inside them.

Verified with the official [yazi-rs/plugins](https://github.com/yazi-rs/plugins)
(commit `c0529fc`), run unmodified: `git`, `full-border`, `no-status`,
`smart-enter`, `chmod`, `jump-to-char`, `smart-filter`, `toggle-pane`, `diff`,
`piper` (with `bat`), `zoom`, `mount`, `vcs-files`, `smart-paste`,
`visual-pivot`, `term-cwd` and `mime-ext`. Not run: `lsar` and `rclone` (need
tools not installed on the test machine), `mactag` (macOS only), and
`compress` (requires yazi ≥ 26.9.1 APIs and 7-Zip).

Built-in versions of the most common functional plugins are available even
without installing them: `plugin fzf`, `plugin zoxide`, `plugin smart-enter`,
`plugin toggle-pane <min|max>-<parent|current|preview>`.

### Remote control

Like `ya emit`, commands can be sent to a running instance from any shell. The
file manager exports `WISP_ID` / `YAZI_ID` (and `YAZI_LEVEL`) to programs
it starts, so inside its shells `emit` targets it automatically:

```bash
wisp emit cd ~/Downloads
wisp emit reveal ~/notes/todo.md
wisp emit-to 12345 tab_create --current
```

## Deep search

Build the index once (incremental afterwards), then press `F` inside the file
manager, or query it from the shell:

```bash
wisp index ~/Documents            # or `wisp index` for your whole home
wisp index ~/Documents --incremental
wisp query "quarterly report"     # ranked results, `--json` for scripts
wisp stats
```

The index covers file names and contents (plain text, code, PDF and DOCX), ranks with BM25 (names weigh more than bodies), handles prefixes
and typos, and supports `type:` / `ext:` filters (`report type:pdf`).

## Development

```bash
cargo test                                   # unit tests + an end-to-end UI test
cargo build --profile quick                  # optimized, fast to rebuild
python3 scripts/screen.py --dir . --keys 'jl' -- "$PWD/target/quick/wisp"
```

`scripts/screen.py` renders any TUI to text through a VT emulator, which is how
the UI is kept identical to yazi's. See [CONTRIBUTING.md](CONTRIBUTING.md) for
the code layout.

The defaults in `crates/cli/preset/` are yazi's own presets (MIT), so the out
of the box behaviour matches yazi exactly.

## License

MIT
